//! `knowledge-observatory-app` — W80-E6B third-party multi-component
//! reference Application (`examples/ecosystem-apps`, Stage E `E6`,
//! `PKG-MULTI-001` / `PKG-SEP-001` / `PKG-IMM-001` / `PKG-SEED-001`).
//!
//! A simulated independent publisher ("observatory-works") driving the full
//! third-party productivity loop over public NLOS authorities only:
//!
//! 1. Build and conformance-check the multi-component `v1.0.0` package
//!    (`curator` executable + `index-sync` background service + read-only
//!    `source-seeds` / `observatory-profile` data entries; three-role
//!    multi-component manifest) and the `v1.1.0` `SameMajor` upgrade tree
//!    (adds the `verify-digest` node).
//! 2. Verify and install the templated package, execute the payload entry,
//!    and read the materialized seed data back from `ArtifactStore`.
//! 3. Enforce lazy-materialization dependency fencing, advance the
//!    `harvest-sources -> build-index -> publish-digest` DAG with
//!    five-dimension `LlmTokenBudget` accounting per step, and publish
//!    content-addressed observatory digest artifacts with causal lineage.
//! 4. Hot-upgrade `v1.0.0 -> v1.1.0` under `CompatibilityWindow::SameMajor`,
//!    evolve the plan to revision 2 (append `verify-digest` while upstream
//!    nodes stay execution-frozen), execute the new node, then uninstall
//!    cleanly and verify fail-closed payload refusal.
//!
//! Usage:
//! - `knowledge-observatory-app` (isolated temporary state root)
//! - `knowledge-observatory-app --root <DIR>` (persist under `<DIR>`)

use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_application::{
    ApplicationStatus, CompatibilityWindow, UninstallApplicationRequest, UninstallDecision,
    UpdateApplicationRequest, UpdateDecision, compile_task_templates,
};
use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageFile,
    PackageFileEntry, PackageManifest, PackageTaskKind, PackageTaskTemplate,
    PackageVerificationReceipt, ProvenanceSourceTriple, PutRevisionRequest, SignedPackageWithTasks,
    SignerDescriptor, VerifyPackageWithTasksRequest, check_package_file, decode_package_file,
    derive_artifact_id, package_manifest_with_tasks_message, validate_task_templates,
};
use nlos_plan::{
    DeclarationAdmissionConsult, DeclarationAdmissionOutcome, MaterializationAdmission,
    MaterializationAdmissionVerdict, MaterializationRequest, MaterializationResolution,
    MaterializationResolutionDecision, NodeTransitionRequest, PlanNodeState, PlanRevisionDecision,
    PlanStoreError, SqlitePlanAuthority,
};
use nlos_resource::{
    ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest,
    LlmTokenBudgetRecord, LlmTokenConsumptionReceipt, LlmTokenLimits, ResourceAuthority,
};
use nlos_slice_k::{
    SliceKError, SliceKRuntime, execute_application_payload, install_plan_revision_key, short_hex,
};
use nlos_task::{SqliteTaskAuthority, TaskStoreError};
use nlos_types::{
    ApplicationId, ArtifactId, IdempotencyKey, OperationId, PackageId, PrincipalId, ReceiptId,
    ResourceAccountId, TaskNodeId, TaskPlanId,
};
use sha2::{Digest, Sha256};

const USAGE: &str = "usage: knowledge-observatory-app [--root <DIR>]";

/// Executable payload entry declared by both manifests.
const EXECUTABLE_ENTRY: &str = "curator";
/// Background-service entry declared by both manifests.
const SERVICE_ENTRY: &str = "index-sync";
/// Seed data entries declared by both manifests.
const PROFILE_ENTRY: &str = "observatory-profile";
const SOURCES_ENTRY: &str = "source-seeds";

/// Deterministic developer signing seed (local sample verification only).
const DEV_SIGNER_SEED: [u8; 32] = [0x6B; 32];
/// `nlos-slice-k` seed band for the `v1.0.0` templated installation.
const SEED_INSTALL_V1: u8 = 0x6F;

/// Node keys declared in `package/package.manifest` (v1.0.0).
const NODE_KEY_HARVEST: &str = "41a1c2d3e4f5061728394a5b6c7d8e01";
const NODE_KEY_BUILD_INDEX: &str = "42b2c3d4e5f60718293a4b5c6d7e8f02";
const NODE_KEY_PUBLISH: &str = "43c3d4e5f60718293a4b5c6d7e8f0303";
/// Node key appended by the `v1.1.0` upgrade manifest.
const NODE_KEY_VERIFY: &str = "44d4e5f60718293a4b5c6d7e8f030404";

/// Domain separators owned by `knowledge-observatory-app`.
const APP_KEY_DOMAIN: &[u8] = b"llmos/knowledge-observatory-app/key/v1";
const DIGEST_ARTIFACT_DOMAIN: &[u8] = b"llmos/knowledge-observatory-app/digest-artifact/v1";
const SIGNER_PROFILE_DOMAIN: &[u8] = b"llmos/knowledge-observatory-app/signer-profile/v1";
const SIGNER_POLICY_DOMAIN: &[u8] = b"llmos/knowledge-observatory-app/signer-policy/v1";
const SIGNER_BOOTSTRAP_DOMAIN: &[u8] = b"llmos/knowledge-observatory-app/signer-bootstrap/v1";

static NEXT_TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
enum AppError {
    Usage,
    Failed(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Usage => formatter.write_str("invalid command-line arguments"),
            Self::Failed(reason) => formatter.write_str(reason),
        }
    }
}

macro_rules! impl_from_error {
    ($source:ty) => {
        impl From<$source> for AppError {
            fn from(source: $source) -> Self {
                Self::Failed(source.to_string())
            }
        }
    };
}

impl_from_error!(SliceKError);
impl_from_error!(nlos_application::ApplicationAuthorityError);
impl_from_error!(nlos_artifact::ArtifactError);
impl_from_error!(nlos_identity::IdentityAuthorityError);
impl_from_error!(nlos_plan::PlanStoreError);
impl_from_error!(nlos_resource::ResourceAuthorityError);
impl_from_error!(nlos_task::TaskStoreError);
impl_from_error!(std::io::Error);

type AppResult<T> = Result<T, AppError>;

/// Durable receipts produced during one end-to-end workflow run.
#[derive(Debug)]
struct WorkflowSummary {
    package_id: PackageId,
    application_id: ApplicationId,
    plan_id: TaskPlanId,
    digest_artifact_id: ArtifactId,
    account_id: ResourceAccountId,
    v1_receipt: ReceiptId,
    v2_receipt: ReceiptId,
    llm_receipts: Vec<LlmTokenConsumptionReceipt>,
    final_budget: LlmTokenBudgetRecord,
    final_digest: ContentDigest,
    uninstall_generation: u64,
}

struct TempStateDir {
    path: PathBuf,
}

impl TempStateDir {
    fn new(tag: &str) -> AppResult<Self> {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "knowledge-observatory-app-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for TempStateDir {
    fn drop(&mut self) {
        for attempt in 0..5 {
            match fs::remove_dir_all(&self.path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound && attempt < 4 => {
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Ok(()) | Err(_) => return,
            }
        }
    }
}

struct ParsedManifestEntry {
    name: String,
    role: PackageEntryRole,
    relative_path: String,
}

struct ParsedManifestTask {
    node_key: [u8; 16],
    kind: PackageTaskKind,
    binding_path: String,
    inputs_path: String,
    outputs_path: String,
    policy_path: String,
    ceiling_path: String,
    dependency_keys: Vec<[u8; 16]>,
}

struct ParsedDeveloperManifest {
    package_id: PackageId,
    version: u64,
    entries: Vec<ParsedManifestEntry>,
    tasks: Vec<ParsedManifestTask>,
}

struct BuiltPackage {
    encoded_bytes: Vec<u8>,
    decoded: PackageFile,
    manifest: PackageManifest,
}

struct TaskDeclarationConsultAdapter<'a> {
    tasks: &'a SqliteTaskAuthority,
}

impl DeclarationAdmissionConsult for TaskDeclarationConsultAdapter<'_> {
    type Error = TaskStoreError;

    fn consult_plan_declaration(
        &self,
        projected_task_nodes: u64,
    ) -> Result<DeclarationAdmissionOutcome, Self::Error> {
        match self.tasks.answer_plan_declaration(projected_task_nodes) {
            Ok(()) => Ok(DeclarationAdmissionOutcome::Admits),
            Err(TaskStoreError::TaskNodeAdmissionDenied {
                profile_id,
                task_count: _,
                max_task_nodes,
            }) => Ok(DeclarationAdmissionOutcome::Denied {
                profile_id: profile_id.to_string(),
                max_task_nodes,
            }),
            Err(other) => Err(other),
        }
    }
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let parsed_root = match parse_root_arg(&arguments) {
        Ok(Some(explicit)) => Some(explicit),
        Ok(None) => None,
        Err(error) => {
            eprintln!("knowledge-observatory-app: {error}");
            eprintln!("{USAGE}");
            return ExitCode::from(error.exit_code());
        }
    };
    let owned_temp;
    let state_root: &Path = if let Some(explicit) = parsed_root.as_deref() {
        explicit
    } else {
        owned_temp = TempStateDir::new("main").expect("temp dir");
        owned_temp.path.as_path()
    };
    match run_observatory_workflow(state_root) {
        Ok(summary) => {
            print_workflow_summary(&summary);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("knowledge-observatory-app: {error}");
            ExitCode::from(2)
        }
    }
}

impl AppError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 1,
            Self::Failed(_) => 2,
        }
    }
}

fn print_workflow_summary(summary: &WorkflowSummary) {
    println!(
        "[observatory] SUMMARY package={} application={} plan={} digest_artifact={} account={} v1_receipt={} v2_receipt={} llm_receipts={} consumed_tokens={} final_digest={} uninstall_generation={}",
        short_hex(summary.package_id.as_bytes()),
        short_hex(summary.application_id.as_bytes()),
        short_hex(summary.plan_id.as_bytes()),
        short_hex(summary.digest_artifact_id.as_bytes()),
        short_hex(summary.account_id.as_bytes()),
        short_hex(summary.v1_receipt.as_bytes()),
        short_hex(summary.v2_receipt.as_bytes()),
        summary.llm_receipts.len(),
        summary.final_budget.consumed_total_tokens,
        short_hex(summary.final_digest.as_bytes()),
        summary.uninstall_generation,
    );
}

fn parse_root_arg(args: &[String]) -> AppResult<Option<PathBuf>> {
    match args {
        [] => Ok(None),
        [flag, dir] if flag == "--root" && !dir.is_empty() && !dir.starts_with("--") => {
            Ok(Some(PathBuf::from(dir)))
        }
        _ => Err(AppError::Usage),
    }
}

fn derive_bytes16(domain: &[u8], phase: &[u8], index: u8) -> [u8; 16] {
    let digest = Sha256::new()
        .chain_update(domain)
        .chain_update(phase)
        .chain_update([index])
        .finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

fn derive_bytes32(domain: &[u8], input: &[u8]) -> [u8; 32] {
    Sha256::new()
        .chain_update(domain)
        .chain_update(input)
        .finalize()
        .into()
}

fn app_key(phase: &[u8], index: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes(derive_bytes16(APP_KEY_DOMAIN, phase, index))
}

fn app_operation_id(phase: &[u8], index: u8) -> OperationId {
    OperationId::from_bytes(derive_bytes16(APP_KEY_DOMAIN, phase, index))
}

fn digest_artifact_id(application_id: ApplicationId, plan_id: TaskPlanId) -> ArtifactId {
    let digest = Sha256::new()
        .chain_update(DIGEST_ARTIFACT_DOMAIN)
        .chain_update(application_id.as_bytes())
        .chain_update(plan_id.as_bytes())
        .finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ArtifactId::from_bytes(id)
}

fn parse_hex16(text: &str, label: &str) -> AppResult<[u8; 16]> {
    if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::Failed(format!(
            "{label} must be 32 hex chars, got {text:?}"
        )));
    }
    let mut out = [0_u8; 16];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16)
            .map_err(|error| AppError::Failed(format!("{label}: {error}")))?;
    }
    Ok(out)
}

fn parse_dotted_version(text: &str) -> AppResult<u64> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.len() != 3 {
        return Err(AppError::Failed(format!(
            "version must be major.minor.patch, got {text:?}"
        )));
    }
    let major: u32 = parts[0]
        .parse()
        .map_err(|_| AppError::Failed(format!("invalid major version in {text:?}")))?;
    let minor: u16 = parts[1]
        .parse()
        .map_err(|_| AppError::Failed(format!("invalid minor version in {text:?}")))?;
    let patch: u16 = parts[2]
        .parse()
        .map_err(|_| AppError::Failed(format!("invalid patch version in {text:?}")))?;
    Ok((u64::from(major) << 32) | (u64::from(minor) << 16) | u64::from(patch))
}

fn parse_developer_manifest(tree: &Path) -> AppResult<ParsedDeveloperManifest> {
    let manifest_path = tree.join("package.manifest");
    let raw = fs::read_to_string(&manifest_path)?;
    let mut package_id = None;
    let mut version = None;
    let mut entries = Vec::new();
    let mut tasks = Vec::new();

    for raw_line in raw.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(AppError::Failed(format!(
                "malformed manifest line in {}: {line:?}",
                manifest_path.display()
            )));
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "package-id" => {
                package_id = Some(PackageId::from_bytes(parse_hex16(value, "package-id")?));
            }
            "version" => {
                version = Some(parse_dotted_version(value)?);
            }
            "entry" => {
                let tokens: Vec<&str> = value.split_whitespace().collect();
                if tokens.len() != 3 {
                    return Err(AppError::Failed(format!("invalid entry line: {value:?}")));
                }
                let role = match tokens[1] {
                    "executable" => PackageEntryRole::Executable,
                    "background-service" => PackageEntryRole::BackgroundService,
                    "data" => PackageEntryRole::Data,
                    other => {
                        return Err(AppError::Failed(format!("unknown entry role {other:?}")));
                    }
                };
                entries.push(ParsedManifestEntry {
                    name: tokens[0].to_string(),
                    role,
                    relative_path: tokens[2].to_string(),
                });
            }
            "task" => {
                let tokens: Vec<&str> = value.split_whitespace().collect();
                if tokens.len() != 7 && tokens.len() != 8 {
                    return Err(AppError::Failed(format!("invalid task line: {value:?}")));
                }
                let node_key = parse_hex16(tokens[0], "task node_key")?;
                let kind = match tokens[1] {
                    "agent-role" => PackageTaskKind::AgentRole,
                    "executable" => PackageTaskKind::Executable,
                    other => return Err(AppError::Failed(format!("unknown task kind {other:?}"))),
                };
                let mut dependency_keys = Vec::new();
                if tokens.len() == 8 {
                    let Some(dep_list) = tokens[7].strip_prefix("deps=") else {
                        return Err(AppError::Failed(format!(
                            "expected deps=<HEX32,...>, got {:?}",
                            tokens[7]
                        )));
                    };
                    for item in dep_list.split(',') {
                        dependency_keys.push(parse_hex16(item, "task dependency")?);
                    }
                }
                tasks.push(ParsedManifestTask {
                    node_key,
                    kind,
                    binding_path: tokens[2].to_string(),
                    inputs_path: tokens[3].to_string(),
                    outputs_path: tokens[4].to_string(),
                    policy_path: tokens[5].to_string(),
                    ceiling_path: tokens[6].to_string(),
                    dependency_keys,
                });
            }
            other => {
                return Err(AppError::Failed(format!(
                    "unknown manifest key {other:?} in {}",
                    manifest_path.display()
                )));
            }
        }
    }

    Ok(ParsedDeveloperManifest {
        package_id: package_id.ok_or_else(|| AppError::Failed("missing package-id".to_string()))?,
        version: version.ok_or_else(|| AppError::Failed("missing version".to_string()))?,
        entries,
        tasks,
    })
}

fn build_and_check_package_tree(tree: &Path) -> AppResult<BuiltPackage> {
    let parsed = parse_developer_manifest(tree)?;
    let signing_key = SigningKey::from_bytes(&DEV_SIGNER_SEED);
    let public_key = signing_key.verifying_key().to_bytes();
    let descriptor = SignerDescriptor {
        public_key,
        profile_digest: derive_bytes32(SIGNER_PROFILE_DOMAIN, &public_key),
        policy_digest: derive_bytes32(SIGNER_POLICY_DOMAIN, &public_key),
        bootstrap_key: derive_bytes16(SIGNER_BOOTSTRAP_DOMAIN, &public_key, 0),
        valid_from_ms: 0,
        valid_until_ms: u64::try_from(i64::MAX).expect("i64::MAX fits u64"),
    };

    let mut entries = Vec::with_capacity(parsed.entries.len());
    for item in &parsed.entries {
        let payload = fs::read(tree.join(&item.relative_path))?;
        entries.push(PackageFileEntry {
            name: item.name.clone(),
            role: item.role,
            artifact_id: derive_artifact_id(parsed.package_id, parsed.version, &item.name),
            digest: ContentDigest::of_bytes(&payload).into_bytes(),
            payload,
        });
    }

    let mut tasks = Vec::with_capacity(parsed.tasks.len());
    for task in &parsed.tasks {
        let file_digest = |rel: &str| -> AppResult<[u8; 32]> {
            let bytes = fs::read(tree.join(rel))?;
            Ok(ContentDigest::of_bytes(&bytes).into_bytes())
        };
        tasks.push(PackageTaskTemplate {
            node_key: task.node_key,
            kind: task.kind,
            binding_digest: file_digest(&task.binding_path)?,
            dependency_keys: task.dependency_keys.clone(),
            input_selectors_digest: file_digest(&task.inputs_path)?,
            output_contract_digest: file_digest(&task.outputs_path)?,
            policy_digest: file_digest(&task.policy_path)?,
            resource_ceiling_digest: file_digest(&task.ceiling_path)?,
        });
    }
    validate_task_templates(&tasks)?;

    let manifest = PackageManifest {
        package_id: parsed.package_id,
        version: parsed.version,
        entries: entries
            .iter()
            .map(PackageFileEntry::manifest_entry)
            .collect(),
    };
    let signing_message = package_manifest_with_tasks_message(&manifest, &tasks);
    let signature = signing_key.sign(&signing_message).to_bytes();

    let package_file = PackageFile {
        descriptor,
        package_id: parsed.package_id,
        version: parsed.version,
        entries,
        tasks,
        signature,
    };
    let encoded_bytes = package_file.encode();
    let report = check_package_file(&encoded_bytes);
    if !report.is_conformant() {
        return Err(AppError::Failed(format!(
            "package conformance check failed for {} with {} finding(s)",
            tree.display(),
            report.findings.len()
        )));
    }
    let decoded = decode_package_file(&encoded_bytes)
        .map_err(|error| AppError::Failed(format!("decode package file: {error}")))?;

    Ok(BuiltPackage {
        encoded_bytes,
        decoded,
        manifest,
    })
}

#[allow(clippy::too_many_lines)]
fn run_observatory_workflow(state_root: &Path) -> AppResult<WorkflowSummary> {
    fs::create_dir_all(state_root)?;
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let v1_tree = manifest_dir.join("package");
    let v2_tree = manifest_dir.join("package-v1.1.0");

    let built_v1 = build_and_check_package_tree(&v1_tree)?;
    let built_v2 = build_and_check_package_tree(&v2_tree)?;
    println!(
        "[observatory] PACKAGES_CONFORMANT v1_entries={} v1_tasks={} v1_bytes={} v2_tasks={} v2_bytes={}",
        built_v1.decoded.entries.len(),
        built_v1.decoded.tasks.len(),
        built_v1.encoded_bytes.len(),
        built_v2.decoded.tasks.len(),
        built_v2.encoded_bytes.len(),
    );
    // Multi-component invariant: all three entry roles present in both trees.
    for (label, built) in [("v1", &built_v1), ("v2", &built_v2)] {
        let roles: Vec<PackageEntryRole> = built
            .decoded
            .entries
            .iter()
            .map(|entry| entry.role)
            .collect();
        assert_eq!(
            roles
                .iter()
                .filter(|role| **role == PackageEntryRole::Executable)
                .count(),
            1,
            "{label} executable count"
        );
        assert_eq!(
            roles
                .iter()
                .filter(|role| **role == PackageEntryRole::BackgroundService)
                .count(),
            1,
            "{label} service count"
        );
        assert!(
            roles
                .iter()
                .filter(|role| **role == PackageEntryRole::Data)
                .count()
                >= 2,
            "{label} data count"
        );
    }

    let runtime = SliceKRuntime::open(state_root)?;
    let resources = ResourceAuthority::open(state_root.join("resources"))?;

    // 1. Verify and install v1.0.0 (ApplicationAuthority generation 1 + plan revision 1).
    let (_principal_v1, signed_pkg_v1, verify_v1) =
        materialize_and_verify_package(&runtime, &built_v1, b"verify-v1")?;
    let templated_install =
        runtime.install_verified_templated_package(&verify_v1, &signed_pkg_v1, SEED_INSTALL_V1)?;
    let installation_v1 = templated_install.installation.clone();
    let plan_rev1 = templated_install.plan_receipt().clone();
    let package_id = installation_v1.package_id;
    let application_id = installation_v1.application_id;
    let plan_id = plan_rev1.plan_id;
    println!(
        "[observatory] INSTALL_V1 application={} generation={} plan_rev={} nodes={}",
        short_hex(application_id.as_bytes()),
        installation_v1.installation_generation.get(),
        plan_rev1.revision,
        plan_rev1.declared_node_count,
    );
    // [PKG-SEP-001] PackageId and ApplicationId stay distinct.
    assert_ne!(package_id.as_bytes(), application_id.as_bytes());

    let payload_v1 = execute_application_payload(&runtime, package_id, EXECUTABLE_ENTRY)?;
    println!(
        "[observatory] PAYLOAD_V1 entry={} operation={} digest={}",
        payload_v1.entry_name,
        short_hex(payload_v1.operation.operation_id.as_bytes()),
        short_hex(payload_v1.payload_digest.as_bytes()),
    );

    // Read the background-service payload and read-only seed entries (PKG-SEED-001).
    let profile_now = runtime.wall_now_ms(app_key(b"seed-read", 0))?;
    let _service_bytes = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        SERVICE_ENTRY,
        profile_now,
    )?;
    let profile_bytes = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        PROFILE_ENTRY,
        profile_now,
    )?;
    let sources_bytes = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        SOURCES_ENTRY,
        profile_now,
    )?;
    let profile_digest = ContentDigest::of_bytes(&profile_bytes);
    let _ = sources_bytes.len();

    // 2. Five-dimension budget for the observatory loop.
    let budget_at = runtime.wall_now_ms(app_key(b"budget", 0))?;
    let account = resources.create_account(CreateAccountRequest {
        initial_credit: 250_000,
        idempotency_key: app_key(b"budget", 1),
        created_at_ms: budget_at,
    })?;
    resources.create_llm_token_budget(CreateLlmTokenBudgetRequest {
        account_id: account.account_id,
        limits: LlmTokenLimits {
            max_prompt_tokens: 12_288,
            max_completion_tokens: 6_144,
            max_total_tokens: 24_576,
            max_api_calls: 12,
            max_cost_microcredits: 90_000,
        },
        idempotency_key: app_key(b"budget", 2),
        created_at_ms: budget_at,
    })?;

    // 3. Dependency gate: publishing before harvesting must fail closed.
    let (harvest_node, harvest_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_HARVEST)?;
    let (index_node, index_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_BUILD_INDEX)?;
    let (publish_node, publish_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_PUBLISH)?;
    let premature_ms = runtime.wall_now_ms(app_key(b"premature", 0))?;
    match runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: index_node,
            idempotency_key: app_key(b"premature", 1),
            requested_at_ms: premature_ms,
        }) {
        Err(PlanStoreError::DependenciesNotReady { unresolved, .. }) => {
            println!(
                "[observatory] PLAN_GATE_BLOCKED node={} unresolved_deps={}",
                short_hex(index_node.as_bytes()),
                unresolved.len(),
            );
        }
        Err(other) => return Err(other.into()),
        Ok(_) => {
            return Err(AppError::Failed(
                "build-index unexpectedly materialized before harvest completed".to_string(),
            ));
        }
    }

    // 4. Execute the three-node DAG with per-step LLM charges and lineage.
    let digest_id = digest_artifact_id(application_id, plan_id);
    let mut upstream_digest = profile_digest;
    let mut llm_receipts = Vec::new();
    let step_plan = [
        (
            "harvest",
            harvest_node,
            harvest_rev,
            "harvest",
            640_u64,
            210,
            90,
            2_800_u64,
        ),
        (
            "index", index_node, index_rev, "index", 980, 520, 240, 5_100,
        ),
        (
            "publish",
            publish_node,
            publish_rev,
            "publish",
            410,
            160,
            40,
            1_900,
        ),
    ];
    let mut revision = 0_u64;
    for (label, node_id, declared_revision, phase, prompt, completion, reasoning, cost) in step_plan
    {
        let phase = phase.as_bytes();
        let _ = label;
        materialize_and_activate_node(&runtime, plan_id, node_id, declared_revision, phase)?;
        let receipt = charge_step_tokens(
            &resources,
            &runtime,
            account.account_id,
            phase,
            prompt,
            completion,
            reasoning,
            cost,
        )?;
        llm_receipts.push(receipt);
        revision += 1;
        let at_ms = runtime.wall_now_ms(app_key(phase, 20))?;
        let body = format!(
            "# Observatory Digest (Revision {revision} - {label})\n\
             - plan: {}\n\
             - node: {label}\n\
             - llm_receipt: {}\n\
             - upstream_digest: {}\n",
            hex_encode(plan_id.as_bytes()),
            hex_encode(receipt.receipt_id.as_bytes()),
            hex_encode(upstream_digest.as_bytes()),
        );
        let provenance = ProvenanceSourceTriple {
            source_a: *node_id.as_bytes(),
            source_b: *receipt.receipt_id.as_bytes(),
            source_digest: upstream_digest,
        };
        if revision == 1 {
            runtime.artifacts.create_artifact(CreateArtifactSpec {
                artifact_id: digest_id,
                idempotency_key: app_key(phase, 21),
                content_type: "text/markdown".to_string(),
                application_id: Some(application_id),
                owner: None,
                created_at_ms: at_ms,
            })?;
        }
        runtime.artifacts.put_revision(PutRevisionRequest {
            artifact_id: digest_id,
            expected_head_revision: revision - 1,
            bytes: body.as_bytes(),
            created_at_ms: at_ms,
            provenance,
        })?;
        complete_active_node(&runtime, plan_id, node_id, declared_revision, phase)?;
        upstream_digest = ContentDigest::of_bytes(body.as_bytes());
    }

    // 5. SameMajor hot upgrade v1.0.0 -> v1.1.0 (generation 1 -> 2) and plan revision 2.
    let (_principal_v2, signed_pkg_v2, verify_v2) =
        materialize_and_verify_package(&runtime, &built_v2, b"verify-v2")?;
    let updated_at = runtime.wall_now_ms(app_key(b"update-v2", 0))?;
    let installation_v2 = match runtime.applications.update_application(
        &runtime.artifacts,
        UpdateApplicationRequest {
            package_id,
            package_verification_receipt_id: verify_v2.receipt_id,
            idempotency_key: app_key(b"update-v2", 1),
            updated_at_ms: updated_at,
            compatibility_window: CompatibilityWindow::SameMajor,
        },
    )? {
        UpdateDecision::Updated(receipt) | UpdateDecision::Replayed(receipt) => receipt,
    };
    println!(
        "[observatory] UPDATE_V1_1 generation={}",
        installation_v2.installation_generation.get(),
    );

    let plan_rev2_at = runtime.wall_now_ms(app_key(b"plan-rev2", 0))?;
    let plan_rev2_key =
        install_plan_revision_key(application_id, installation_v2.installation_generation);
    let consult = TaskDeclarationConsultAdapter {
        tasks: &runtime.tasks,
    };
    let mut plan_rev2_req = compile_task_templates(&signed_pkg_v2, plan_rev2_key, plan_rev2_at)
        .map_err(|error| AppError::Failed(format!("compile v1.1.0 task templates: {error:?}")))?;
    plan_rev2_req.plan_id = Some(plan_id);
    let plan_rev2_receipt = match runtime
        .plans()
        .apply_plan_revision_with_admission(plan_rev2_req, &consult)?
    {
        PlanRevisionDecision::Applied(receipt) | PlanRevisionDecision::Replayed(receipt) => receipt,
    };
    let chain = runtime.plans().verify_revision_chain(plan_id)?;
    println!(
        "[observatory] PLAN_REV2 revision={} chain_len={} nodes={}",
        plan_rev2_receipt.revision, chain.revision_count, plan_rev2_receipt.declared_node_count,
    );

    // 6. Execute the appended verify-digest node and publish revision 4.
    let (verify_node, verify_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_VERIFY)?;
    materialize_and_activate_node(&runtime, plan_id, verify_node, verify_rev, b"step-verify")?;
    let verify_receipt = charge_step_tokens(
        &resources,
        &runtime,
        account.account_id,
        b"step-verify",
        150,
        80,
        20,
        900,
    )?;
    llm_receipts.push(verify_receipt);
    let rev4_at = runtime.wall_now_ms(app_key(b"step-verify", 22))?;
    let rev4_body = format!(
        "# Observatory Digest (Revision 4 - verify)\n\
         - upstream_digest: {}\n\
         - verifier_node: verify-digest\n\
         - status: VERIFIED\n",
        hex_encode(upstream_digest.as_bytes()),
    );
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id: digest_id,
        expected_head_revision: 3,
        bytes: rev4_body.as_bytes(),
        created_at_ms: rev4_at,
        provenance: ProvenanceSourceTriple {
            source_a: *verify_node.as_bytes(),
            source_b: *verify_receipt.receipt_id.as_bytes(),
            source_digest: upstream_digest,
        },
    })?;
    complete_active_node(&runtime, plan_id, verify_node, verify_rev, b"step-verify")?;
    let final_digest = ContentDigest::of_bytes(rev4_body.as_bytes());

    let final_budget = resources.inspect_llm_token_budget(account.account_id)?;
    let uninstalled_at = runtime.wall_now_ms(app_key(b"uninstall", 0))?;
    let uninstall_receipt =
        match runtime
            .applications
            .uninstall_application(UninstallApplicationRequest {
                package_id,
                idempotency_key: app_key(b"uninstall", 1),
                uninstalled_at_ms: uninstalled_at,
            })? {
            UninstallDecision::Uninstalled(receipt) | UninstallDecision::Replayed(receipt) => {
                receipt
            }
        };
    let app_after = runtime
        .applications
        .inspect_application(package_id)?
        .ok_or_else(|| AppError::Failed("application row missing after uninstall".to_string()))?;
    if app_after.status != ApplicationStatus::Uninstalled {
        return Err(AppError::Failed(
            "application status must be Uninstalled after clean uninstall".to_string(),
        ));
    }
    if execute_application_payload(&runtime, package_id, EXECUTABLE_ENTRY).is_ok() {
        return Err(AppError::Failed(
            "uninstalled application must refuse payload execution".to_string(),
        ));
    }
    println!(
        "[observatory] UNINSTALL generation={} status=uninstalled",
        uninstall_receipt.application_generation.get(),
    );

    Ok(WorkflowSummary {
        package_id,
        application_id,
        plan_id,
        digest_artifact_id: digest_id,
        account_id: account.account_id,
        v1_receipt: verify_v1.receipt_id,
        v2_receipt: verify_v2.receipt_id,
        llm_receipts,
        final_budget,
        final_digest,
        uninstall_generation: uninstall_receipt.application_generation.get(),
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn read_seed_entry(
    artifacts: &ArtifactStore,
    package_id: PackageId,
    package_version: u64,
    entry_name: &str,
    now_ms: u64,
) -> AppResult<Vec<u8>> {
    let artifact_id =
        ArtifactId::from_bytes(derive_artifact_id(package_id, package_version, entry_name));
    Ok(artifacts.get_revision(artifact_id, 1, now_ms)?)
}

fn materialize_and_verify_package(
    runtime: &SliceKRuntime,
    built: &BuiltPackage,
    phase: &[u8],
) -> AppResult<(
    PrincipalId,
    SignedPackageWithTasks,
    PackageVerificationReceipt,
)> {
    let signer_principal = runtime
        .identity
        .bootstrap_principal(built.decoded.descriptor.bootstrap_request())?
        .binding()
        .principal_id;
    let at_ms = runtime.wall_now_ms(app_key(phase, 0))?;

    for (idx, entry) in built.decoded.entries.iter().enumerate() {
        let entry_idx = u8::try_from(idx + 1).unwrap_or(u8::MAX);
        let artifact_id = ArtifactId::from_bytes(entry.artifact_id);
        runtime.artifacts.create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: app_key(phase, entry_idx),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })?;
        runtime.artifacts.put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &entry.payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: entry.artifact_id,
                source_b: *built.decoded.package_id.as_bytes(),
                source_digest: ContentDigest::from_bytes(entry.digest),
            },
        })?;
    }

    let signed_package = SignedPackageWithTasks {
        manifest: built.manifest.clone(),
        tasks: built.decoded.tasks.clone(),
        signer: signer_principal,
        signature: built.decoded.signature,
    };
    let decision = runtime.artifacts.verify_package_with_tasks(
        &runtime.identity,
        VerifyPackageWithTasksRequest {
            signed: &signed_package,
            idempotency_key: app_key(phase, 30),
            verified_at_ms: at_ms,
        },
    )?;
    Ok((signer_principal, signed_package, decision.receipt().clone()))
}

fn resolve_node_id_by_key(
    plans: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_key_hex: &str,
) -> AppResult<(TaskNodeId, u64)> {
    let target_key = parse_hex16(node_key_hex, "node_key")?;
    for record in plans.list_plan_nodes(plan_id)? {
        if record.node_key == target_key {
            return Ok((record.node_id, record.declared_revision));
        }
    }
    Err(AppError::Failed(format!(
        "plan node {node_key_hex} not found in plan {}",
        short_hex(plan_id.as_bytes())
    )))
}

fn materialize_and_activate_node(
    runtime: &SliceKRuntime,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    phase: &[u8],
) -> AppResult<()> {
    let request_key = app_key(phase, 1);
    let requested_at = runtime.wall_now_ms(app_key(phase, 2))?;
    let _ = runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: request_key,
            requested_at_ms: requested_at,
        })?;
    let projected_nodes = runtime.plans().inspect_declared_task_node_count()?;
    let resolved_at = runtime.wall_now_ms(app_key(phase, 3))?;
    let decision = runtime
        .plans()
        .resolve_materialization(MaterializationResolution {
            request_key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "observatory-local-workstation".to_string(),
                projected_task_nodes: projected_nodes,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: resolved_at,
        })?;
    if !matches!(
        decision,
        MaterializationResolutionDecision::Approved(_)
            | MaterializationResolutionDecision::ReplayedApproved(_)
    ) {
        return Err(AppError::Failed(
            "expected materialization gate approval".to_string(),
        ));
    }
    let active_at = runtime.wall_now_ms(app_key(phase, 4))?;
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 5),
            transitioned_at_ms: active_at,
        })?;
    Ok(())
}

fn complete_active_node(
    runtime: &SliceKRuntime,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    phase: &[u8],
) -> AppResult<()> {
    let completed_at = runtime.wall_now_ms(app_key(phase, 6))?;
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Active,
            to_state: PlanNodeState::Completed,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 7),
            transitioned_at_ms: completed_at,
        })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn charge_step_tokens(
    resources: &ResourceAuthority,
    runtime: &SliceKRuntime,
    account_id: ResourceAccountId,
    phase: &[u8],
    prompt_tokens: u64,
    completion_tokens: u64,
    reasoning_tokens: u64,
    cost_microcredits: u64,
) -> AppResult<LlmTokenConsumptionReceipt> {
    let consumed_at = runtime.wall_now_ms(app_key(phase, 10))?;
    let receipt = resources
        .consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id,
            operation_id: app_operation_id(phase, 11),
            prompt_tokens,
            completion_tokens,
            reasoning_tokens,
            api_calls: 1,
            cost_microcredits,
            idempotency_key: app_key(phase, 12),
            consumed_at_ms: consumed_at,
        })?
        .receipt();
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nlos_artifact::{HeadState, check_package_file};
    use nlos_resource::{LlmBudgetDimension, LlmTokenConsumeDecision, ResourceAuthorityError};
    use nlos_slice_k::SliceKRuntime;
    use nlos_types::Generation;

    use super::{
        NODE_KEY_BUILD_INDEX, NODE_KEY_HARVEST, NODE_KEY_PUBLISH, NODE_KEY_VERIFY, SERVICE_ENTRY,
        TempStateDir, app_key, build_and_check_package_tree, parse_root_arg,
        resolve_node_id_by_key, run_observatory_workflow,
    };

    #[test]
    fn cli_argument_parser_accepts_default_and_explicit_root_only() {
        assert_eq!(parse_root_arg(&[]).expect("empty args"), None);
        let explicit =
            parse_root_arg(&["--root".to_string(), "trial-state".to_string()]).expect("root");
        assert_eq!(explicit.as_deref(), Some(Path::new("trial-state")));
        assert!(parse_root_arg(&["--unknown".to_string()]).is_err());
        assert!(parse_root_arg(&["--root".to_string(), "--bad".to_string()]).is_err());
    }

    #[test]
    fn multi_component_trees_pass_conformance_determinism_and_tamper_detection() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let v1 = build_and_check_package_tree(&manifest_dir.join("package")).expect("build v1.0.0");
        let v2 = build_and_check_package_tree(&manifest_dir.join("package-v1.1.0"))
            .expect("build v1.1.0");

        assert_eq!(v1.decoded.package_id, v2.decoded.package_id);
        assert_ne!(v1.decoded.version, v2.decoded.version);
        // Multi-component: executable + background-service + driver bridge +
        // ui surface card + >=2 read-only seeds (PKG-MULTI-001).
        assert_eq!(v1.decoded.entries.len(), 6);
        assert_eq!(v2.decoded.entries.len(), 6);
        assert!(
            v1.decoded
                .entries
                .iter()
                .any(|entry| entry.name == SERVICE_ENTRY)
        );
        assert_eq!(v1.decoded.tasks.len(), 3);
        assert_eq!(v2.decoded.tasks.len(), 4);

        let rebuilt =
            build_and_check_package_tree(&manifest_dir.join("package")).expect("rebuild v1.0.0");
        assert_eq!(v1.encoded_bytes, rebuilt.encoded_bytes);

        let mut tampered = v1.encoded_bytes.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xFF;
        assert!(!check_package_file(&tampered).is_conformant());
    }

    #[test]
    fn end_to_end_workflow_upgrades_and_uninstalls_cleanly() {
        let temp = TempStateDir::new("e2e").expect("temp dir");
        let summary = run_observatory_workflow(&temp.path).expect("run observatory workflow");
        assert_eq!(summary.uninstall_generation, 2);
        // 3 base steps + 1 upgrade step = 4 receipts, 2_180 prompt + 970 completion + 390 reasoning.
        assert_eq!(summary.llm_receipts.len(), 4);
        assert_eq!(summary.final_budget.consumed_prompt_tokens, 2_180);
        assert_eq!(summary.final_budget.consumed_completion_tokens, 970);
        assert_eq!(summary.final_budget.consumed_reasoning_tokens, 390);
        assert_eq!(summary.final_budget.consumed_total_tokens, 3_540);
        assert_eq!(summary.final_budget.consumed_api_calls, 4);
        assert_eq!(summary.final_budget.consumed_cost_microcredits, 10_700);
        assert_eq!(summary.final_budget.remaining_api_calls(), 8);

        let runtime = SliceKRuntime::open(&temp.path).expect("reopen runtime");
        let head: HeadState = runtime
            .artifacts
            .resolve_head(summary.digest_artifact_id, u64::MAX)
            .expect("resolve digest head")
            .expect("digest head present");
        assert_eq!(head.revision, 4);
        assert_eq!(head.digest, summary.final_digest);

        for node_hex in [
            NODE_KEY_HARVEST,
            NODE_KEY_BUILD_INDEX,
            NODE_KEY_PUBLISH,
            NODE_KEY_VERIFY,
        ] {
            let (node_id, _) =
                resolve_node_id_by_key(runtime.plans(), summary.plan_id, node_hex).expect("node");
            let record = runtime
                .plans()
                .inspect_node(summary.plan_id, node_id)
                .expect("inspect node")
                .expect("node exists");
            assert_eq!(record.state, nlos_plan::PlanNodeState::Completed);
        }
    }

    #[test]
    fn budget_exhaustion_fails_closed_on_the_prompt_dimension() {
        let temp = TempStateDir::new("neg-budget").expect("temp dir");
        let resources =
            nlos_resource::ResourceAuthority::open(temp.path.join("resources")).expect("open");
        let account = resources
            .create_account(nlos_resource::CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: app_key(b"neg", 0),
                created_at_ms: 100,
            })
            .expect("account");
        resources
            .create_llm_token_budget(nlos_resource::CreateLlmTokenBudgetRequest {
                account_id: account.account_id,
                limits: nlos_resource::LlmTokenLimits {
                    max_prompt_tokens: 300,
                    max_completion_tokens: 300,
                    max_total_tokens: 500,
                    max_api_calls: 2,
                    max_cost_microcredits: 5_000,
                },
                idempotency_key: app_key(b"neg", 1),
                created_at_ms: 100,
            })
            .expect("budget");
        let over = resources.consume_llm_tokens(nlos_resource::ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: super::app_operation_id(b"neg", 2),
            prompt_tokens: 400,
            completion_tokens: 50,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 1_000,
            idempotency_key: app_key(b"neg", 3),
            consumed_at_ms: 110,
        });
        assert!(matches!(
            over,
            Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
                dimension: LlmBudgetDimension::PromptTokens,
                ..
            })
        ));
        // Idempotent replay of a consumed request still works after exhaustion.
        let ok = resources
            .consume_llm_tokens(nlos_resource::ConsumeLlmTokensRequest {
                account_id: account.account_id,
                operation_id: super::app_operation_id(b"neg", 4),
                prompt_tokens: 200,
                completion_tokens: 50,
                reasoning_tokens: 0,
                api_calls: 1,
                cost_microcredits: 1_000,
                idempotency_key: app_key(b"neg", 5),
                consumed_at_ms: 120,
            })
            .expect("within budget");
        let replay = resources
            .consume_llm_tokens(nlos_resource::ConsumeLlmTokensRequest {
                account_id: account.account_id,
                operation_id: super::app_operation_id(b"neg", 4),
                prompt_tokens: 200,
                completion_tokens: 50,
                reasoning_tokens: 0,
                api_calls: 1,
                cost_microcredits: 1_000,
                idempotency_key: app_key(b"neg", 5),
                consumed_at_ms: 130,
            })
            .expect("replay");
        assert!(matches!(replay, LlmTokenConsumeDecision::Replayed(_)));
        assert_eq!(
            ok.receipt().receipt_id,
            replay.receipt().receipt_id,
            "replayed receipt must match the original"
        );
        let _ = Generation::INITIAL;
    }
}
