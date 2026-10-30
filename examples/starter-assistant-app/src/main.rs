//! `starter-assistant-app` — Stage E `E0` (`W77-PRE-C`) out-of-the-box reference
//! productivity application workflow over public NLOS authorities only.
//!
//! Demonstrates and verifies the complete third-party productivity loop without
//! touching any private kernel internals:
//!
//! 1. Build, conformance-check, verify, and install the signed task-templated
//!    `v1.0.0` package (`draft-plan -> synthesize-artifact`) into
//!    `IdentityAuthority`, `ArtifactStore`, `ApplicationAuthority`, and
//!    `SqlitePlanAuthority`.
//! 2. Initialize a `ResourceAuthority` account with a five-dimension
//!    `LlmTokenBudget` and charge checked-integer model token usage per step,
//!    producing immutable `LlmTokenConsumptionReceipt` records.
//! 3. Enforce lazy materialization dependency fencing (`[PLAN-LAZY-001]`),
//!    advance the `TaskPlan` DAG nodes through `Materializing -> Active ->
//!    Completed`, and write/revise a content-addressed `ArtifactStore` brief
//!    with causal `ProvenanceSourceTriple` lineage across revisions.
//! 4. Perform a `v1.0.0 -> v1.1.0` `CompatibilityWindow::SameMajor` hot upgrade,
//!    execute the generation-2 payload, append `TaskPlan` revision 2 adding
//!    `verify-brief` while preserving execution-frozen upstream nodes
//!    (`[PLAN-DAG-001]` G1), publish brief revision 3, and cleanly uninstall.
//!
//! Usage:
//! - `starter-assistant-app` (runs against an isolated temporary state root)
//! - `starter-assistant-app --root <DIR>` (persists authorities under `<DIR>`)

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

const USAGE: &str = "usage: starter-assistant-app [--root <DIR>]";

/// Executable entry declared in both `v1.0.0` and `v1.1.0` manifests.
const EXECUTABLE_ENTRY: &str = "assistant-driver";
/// Data profile entry declared in both `v1.0.0` and `v1.1.0` manifests.
const PROFILE_ENTRY: &str = "assistant-profile";

/// Deterministic developer signing seed used solely for local sample verification.
const DEV_SIGNER_SEED: [u8; 32] = [0x47_u8; 32];
/// `nlos-slice-k` seed band for `v1.0.0` templated installation.
const SEED_INSTALL_V1: u8 = 0x3A;

/// Node keys declared in `package/package.manifest` and `package-v1.1.0/package.manifest`.
const NODE_KEY_DRAFT_PLAN: &str = "10a1c2d3e4f5061728394a5b6c7d8e01";
const NODE_KEY_SYNTHESIZE_ARTIFACT: &str = "20b2c3d4e5f60718293a4b5c6d7e8f02";
const NODE_KEY_VERIFY_BRIEF: &str = "30c3d4e5f60718293a4b5c6d7e8f0303";

/// Domain separators owned by `starter-assistant-app`.
const APP_KEY_DOMAIN: &[u8] = b"llmos/starter-assistant-app/key/v1";
const BRIEF_ARTIFACT_DOMAIN: &[u8] = b"llmos/starter-assistant-app/brief-artifact/v1";
const SIGNER_PROFILE_DOMAIN: &[u8] = b"llmos/starter-assistant-app/signer-profile/v1";
const SIGNER_POLICY_DOMAIN: &[u8] = b"llmos/starter-assistant-app/signer-policy/v1";
const SIGNER_BOOTSTRAP_DOMAIN: &[u8] = b"llmos/starter-assistant-app/signer-bootstrap/v1";

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

/// Summary of all durable receipts produced during one end-to-end workflow run.
#[derive(Debug)]
struct WorkflowSummary {
    package_id: PackageId,
    application_id: ApplicationId,
    plan_id: TaskPlanId,
    brief_artifact_id: ArtifactId,
    account_id: ResourceAccountId,
    v1_verification_receipt: ReceiptId,
    v2_verification_receipt: ReceiptId,
    v1_payload_digest: ContentDigest,
    v2_payload_digest: ContentDigest,
    llm_receipts: Vec<LlmTokenConsumptionReceipt>,
    final_budget: LlmTokenBudgetRecord,
    final_brief_digest: ContentDigest,
    uninstall_generation: u64,
}

struct TempStateDir {
    path: PathBuf,
}

impl TempStateDir {
    fn new(tag: &str) -> AppResult<Self> {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "starter-assistant-app-{tag}-{}-{seq}",
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

struct BuiltStarterPackage {
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
    match parse_root_arg(&std::env::args().skip(1).collect::<Vec<_>>()) {
        Ok(Some(explicit_root)) => match run_productivity_workflow(&explicit_root) {
            Ok(summary) => {
                print_workflow_summary(&summary);
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("starter-assistant-app: {error}");
                ExitCode::from(2)
            }
        },
        Ok(None) => {
            let temp = match TempStateDir::new("main") {
                Ok(dir) => dir,
                Err(error) => {
                    eprintln!("starter-assistant-app: {error}");
                    return ExitCode::from(2);
                }
            };
            match run_productivity_workflow(&temp.path) {
                Ok(summary) => {
                    print_workflow_summary(&summary);
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    eprintln!("starter-assistant-app: {error}");
                    ExitCode::from(2)
                }
            }
        }
        Err(AppError::Usage) => {
            eprintln!("{USAGE}");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("starter-assistant-app: {error}");
            ExitCode::from(2)
        }
    }
}

fn print_workflow_summary(summary: &WorkflowSummary) {
    println!(
        "[starter-assistant] SUMMARY package={} application={} plan={} brief_artifact={} account={} v1_receipt={} v2_receipt={} v1_payload_digest={} v2_payload_digest={} llm_receipts={} consumed_tokens={} final_brief_digest={} uninstall_generation={}",
        short_hex(summary.package_id.as_bytes()),
        short_hex(summary.application_id.as_bytes()),
        short_hex(summary.plan_id.as_bytes()),
        short_hex(summary.brief_artifact_id.as_bytes()),
        short_hex(summary.account_id.as_bytes()),
        short_hex(summary.v1_verification_receipt.as_bytes()),
        short_hex(summary.v2_verification_receipt.as_bytes()),
        short_hex(summary.v1_payload_digest.as_bytes()),
        short_hex(summary.v2_payload_digest.as_bytes()),
        summary.llm_receipts.len(),
        summary.final_budget.consumed_total_tokens,
        short_hex(summary.final_brief_digest.as_bytes()),
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

fn brief_artifact_id(application_id: ApplicationId, plan_id: TaskPlanId) -> ArtifactId {
    let digest = Sha256::new()
        .chain_update(BRIEF_ARTIFACT_DOMAIN)
        .chain_update(application_id.as_bytes())
        .chain_update(plan_id.as_bytes())
        .finalize();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ArtifactId::from_bytes(id)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
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

fn build_and_check_package_tree(tree: &Path) -> AppResult<BuiltStarterPackage> {
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

    Ok(BuiltStarterPackage {
        encoded_bytes,
        decoded,
        manifest,
    })
}

fn materialize_and_verify_package(
    runtime: &SliceKRuntime,
    built: &BuiltStarterPackage,
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
    let requested_at_ms = runtime.wall_now_ms(app_key(phase, 2))?;
    let _ = runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id,
            idempotency_key: request_key,
            requested_at_ms,
        })?;

    let projected_nodes = runtime.plans().inspect_declared_task_node_count()?;
    let resolved_at_ms = runtime.wall_now_ms(app_key(phase, 3))?;
    let decision = runtime
        .plans()
        .resolve_materialization(MaterializationResolution {
            request_key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "starter-local-workstation".to_string(),
                projected_task_nodes: projected_nodes,
                projected_active_working_set: 1,
            }),
            resolved_at_ms,
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

    let active_at_ms = runtime.wall_now_ms(app_key(phase, 4))?;
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 5),
            transitioned_at_ms: active_at_ms,
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
    let completed_at_ms = runtime.wall_now_ms(app_key(phase, 6))?;
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Active,
            to_state: PlanNodeState::Completed,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 7),
            transitioned_at_ms: completed_at_ms,
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
    let consumed_at_ms = runtime.wall_now_ms(app_key(phase, 10))?;
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
            consumed_at_ms,
        })?
        .receipt();
    println!(
        "[starter-assistant] LLM_RECEIPT seq={} id={} total_tokens={} cumulative_tokens={} cumulative_calls={} cumulative_cost_uc={}",
        receipt.sequence,
        short_hex(receipt.receipt_id.as_bytes()),
        receipt.total_tokens,
        receipt.cumulative_total_tokens,
        receipt.cumulative_api_calls,
        receipt.cumulative_cost_microcredits,
    );
    Ok(receipt)
}

fn verify_and_read_artifact_revision(
    artifacts: &ArtifactStore,
    artifact_id: ArtifactId,
    revision: u64,
    now_ms: u64,
) -> AppResult<(Vec<u8>, ContentDigest)> {
    let bytes = artifacts.get_revision(artifact_id, revision, now_ms)?;
    let provenance = artifacts.inspect_provenance(artifact_id, revision)?;
    let digest = ContentDigest::of_bytes(&bytes);
    println!(
        "[starter-assistant] ARTIFACT_REV artifact={} revision={} bytes={} digest={} provenance_receipt={}",
        short_hex(artifact_id.as_bytes()),
        revision,
        bytes.len(),
        short_hex(digest.as_bytes()),
        short_hex(provenance.receipt_id.as_bytes()),
    );
    Ok((bytes, digest))
}

#[allow(clippy::too_many_lines)]
fn run_productivity_workflow(state_root: &Path) -> AppResult<WorkflowSummary> {
    fs::create_dir_all(state_root)?;
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let v1_tree = manifest_dir.join("package");
    let v2_tree = manifest_dir.join("package-v1.1.0");

    let built_v1 = build_and_check_package_tree(&v1_tree)?;
    let built_v2 = build_and_check_package_tree(&v2_tree)?;
    println!(
        "[starter-assistant] PACKAGES_CONFORMANT v1_bytes={} v2_bytes={}",
        built_v1.encoded_bytes.len(),
        built_v2.encoded_bytes.len(),
    );

    let runtime = SliceKRuntime::open(state_root)?;
    let resources = ResourceAuthority::open(state_root.join("resources"))?;

    // 1. Verify and install v1.0.0 templated package (ApplicationAuthority gen 1 + TaskPlan rev 1).
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
        "[starter-assistant] INSTALL_V1 application={} package={} generation={} plan={} plan_rev={} declared_nodes={}",
        short_hex(application_id.as_bytes()),
        short_hex(package_id.as_bytes()),
        installation_v1.installation_generation.get(),
        short_hex(plan_id.as_bytes()),
        plan_rev1.revision,
        plan_rev1.declared_node_count,
    );

    let payload_v1 = execute_application_payload(&runtime, package_id, EXECUTABLE_ENTRY)?;
    println!(
        "[starter-assistant] PAYLOAD_V1 entry={} generation={} operation={} digest={}",
        payload_v1.entry_name,
        payload_v1.installation_generation.get(),
        short_hex(payload_v1.operation.operation_id.as_bytes()),
        short_hex(payload_v1.payload_digest.as_bytes()),
    );

    // Read materialized assistant-profile data entry from ArtifactStore.
    let profile_artifact_id = ArtifactId::from_bytes(derive_artifact_id(
        package_id,
        installation_v1.package_version,
        PROFILE_ENTRY,
    ));
    let profile_now_ms = runtime.wall_now_ms(app_key(b"profile-read", 0))?;
    let profile_bytes = runtime
        .artifacts
        .get_revision(profile_artifact_id, 1, profile_now_ms)?;
    let profile_digest = ContentDigest::of_bytes(&profile_bytes);

    // 2. Establish ResourceAuthority five-dimension LlmTokenBudget.
    let account_created_ms = runtime.wall_now_ms(app_key(b"budget", 0))?;
    let account = resources.create_account(CreateAccountRequest {
        initial_credit: 100_000,
        idempotency_key: app_key(b"budget", 1),
        created_at_ms: account_created_ms,
    })?;
    let budget_record = resources
        .create_llm_token_budget(CreateLlmTokenBudgetRequest {
            account_id: account.account_id,
            limits: LlmTokenLimits {
                max_prompt_tokens: 8_192,
                max_completion_tokens: 4_096,
                max_total_tokens: 16_384,
                max_api_calls: 8,
                max_cost_microcredits: 50_000,
            },
            idempotency_key: app_key(b"budget", 2),
            created_at_ms: account_created_ms,
        })?
        .record();
    println!(
        "[starter-assistant] BUDGET_INIT account={} generation={} max_total_tokens={} max_api_calls={} max_cost_uc={}",
        short_hex(account.account_id.as_bytes()),
        budget_record.generation.get(),
        budget_record.limits.max_total_tokens,
        budget_record.limits.max_api_calls,
        budget_record.limits.max_cost_microcredits,
    );

    // 3. Enforce [PLAN-LAZY-001] dependency readiness gate on `synthesize-artifact` before `draft-plan`.
    let (draft_node_id, draft_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_DRAFT_PLAN)?;
    let (synth_node_id, synth_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_SYNTHESIZE_ARTIFACT)?;

    let premature_ms = runtime.wall_now_ms(app_key(b"premature-synth", 0))?;
    match runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: synth_node_id,
            idempotency_key: app_key(b"premature-synth", 1),
            requested_at_ms: premature_ms,
        }) {
        Err(PlanStoreError::DependenciesNotReady { unresolved, .. }) => {
            println!(
                "[starter-assistant] PLAN_GATE_BLOCKED node={} unresolved_deps={}",
                short_hex(synth_node_id.as_bytes()),
                unresolved.len(),
            );
        }
        Err(other) => return Err(other.into()),
        Ok(_) => {
            return Err(AppError::Failed(
                "synthesize-artifact unexpectedly materialized before draft-plan completed"
                    .to_string(),
            ));
        }
    }

    // 4. Execute Node 1 (`draft-plan`) -> charge LLM tokens -> create & write Artifact revision 1.
    materialize_and_activate_node(&runtime, plan_id, draft_node_id, draft_rev, b"node1-draft")?;
    let llm_receipt_1 = charge_step_tokens(
        &resources,
        &runtime,
        account.account_id,
        b"llm-step1",
        540,
        280,
        120,
        3_200,
    )?;

    let brief_id = brief_artifact_id(application_id, plan_id);
    let rev1_ms = runtime.wall_now_ms(app_key(b"brief-rev1", 0))?;
    runtime.artifacts.create_artifact(CreateArtifactSpec {
        artifact_id: brief_id,
        idempotency_key: app_key(b"brief-rev1", 1),
        content_type: "text/markdown".to_string(),
        application_id: Some(application_id),
        owner: None,
        created_at_ms: rev1_ms,
    })?;
    let rev1_body = format!(
        "# NLOS Productivity Brief (Revision 1 - Draft Outline)\n\
         - plan_id: {}\n\
         - producer_node: {}\n\
         - llm_receipt: {}\n\
         - profile_digest: {}\n",
        hex_encode(plan_id.as_bytes()),
        hex_encode(draft_node_id.as_bytes()),
        hex_encode(llm_receipt_1.receipt_id.as_bytes()),
        hex_encode(profile_digest.as_bytes()),
    );
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id: brief_id,
        expected_head_revision: 0,
        bytes: rev1_body.as_bytes(),
        created_at_ms: rev1_ms,
        provenance: ProvenanceSourceTriple {
            source_a: *draft_node_id.as_bytes(),
            source_b: *llm_receipt_1.receipt_id.as_bytes(),
            source_digest: profile_digest,
        },
    })?;
    complete_active_node(&runtime, plan_id, draft_node_id, draft_rev, b"node1-draft")?;
    let (rev1_readback, rev1_digest) =
        verify_and_read_artifact_revision(&runtime.artifacts, brief_id, 1, rev1_ms)?;

    // 5. Execute Node 2 (`synthesize-artifact`) -> read rev 1 -> charge LLM tokens -> publish rev 2.
    materialize_and_activate_node(&runtime, plan_id, synth_node_id, synth_rev, b"node2-synth")?;
    let llm_receipt_2 = charge_step_tokens(
        &resources,
        &runtime,
        account.account_id,
        b"llm-step2",
        890,
        610,
        190,
        5_400,
    )?;
    let rev2_ms = runtime.wall_now_ms(app_key(b"brief-rev2", 0))?;
    let rev2_body = format!(
        "{}\n## Synthesized Analysis (Revision 2)\n\
         - upstream_revision_digest: {}\n\
         - synthesizer_node: {}\n\
         - llm_receipt: {}\n",
        String::from_utf8_lossy(&rev1_readback).trim_end(),
        hex_encode(rev1_digest.as_bytes()),
        hex_encode(synth_node_id.as_bytes()),
        hex_encode(llm_receipt_2.receipt_id.as_bytes()),
    );
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id: brief_id,
        expected_head_revision: 1,
        bytes: rev2_body.as_bytes(),
        created_at_ms: rev2_ms,
        provenance: ProvenanceSourceTriple {
            source_a: *synth_node_id.as_bytes(),
            source_b: *llm_receipt_2.receipt_id.as_bytes(),
            source_digest: rev1_digest,
        },
    })?;
    complete_active_node(&runtime, plan_id, synth_node_id, synth_rev, b"node2-synth")?;
    let (rev2_readback, rev2_digest) =
        verify_and_read_artifact_revision(&runtime.artifacts, brief_id, 2, rev2_ms)?;

    // 6. Hot-upgrade v1.0.0 -> v1.1.0 under CompatibilityWindow::SameMajor (generation 1 -> 2).
    let (_principal_v2, signed_pkg_v2, verify_v2) =
        materialize_and_verify_package(&runtime, &built_v2, b"verify-v2")?;
    let updated_at_ms = runtime.wall_now_ms(app_key(b"update-v2", 0))?;
    let installation_v2 = match runtime.applications.update_application(
        &runtime.artifacts,
        UpdateApplicationRequest {
            package_id,
            package_verification_receipt_id: verify_v2.receipt_id,
            idempotency_key: app_key(b"update-v2", 1),
            updated_at_ms,
            compatibility_window: CompatibilityWindow::SameMajor,
        },
    )? {
        UpdateDecision::Updated(receipt) | UpdateDecision::Replayed(receipt) => receipt,
    };
    let payload_v2 = execute_application_payload(&runtime, package_id, EXECUTABLE_ENTRY)?;
    println!(
        "[starter-assistant] UPDATE_V1_1 generation={} payload_operation={} payload_digest={}",
        installation_v2.installation_generation.get(),
        short_hex(payload_v2.operation.operation_id.as_bytes()),
        short_hex(payload_v2.payload_digest.as_bytes()),
    );

    // Evolve the TaskPlan to revision 2 (preserving frozen nodes 1 & 2, appending `verify-brief`).
    let plan_rev2_ms = runtime.wall_now_ms(app_key(b"plan-rev2", 0))?;
    let plan_rev2_key =
        install_plan_revision_key(application_id, installation_v2.installation_generation);
    let mut plan_rev2_req = compile_task_templates(&signed_pkg_v2, plan_rev2_key, plan_rev2_ms)
        .map_err(|error| AppError::Failed(format!("compile v1.1.0 task templates: {error:?}")))?;
    plan_rev2_req.plan_id = Some(plan_id);
    let consult = TaskDeclarationConsultAdapter {
        tasks: &runtime.tasks,
    };
    let plan_rev2_receipt = match runtime
        .plans()
        .apply_plan_revision_with_admission(plan_rev2_req, &consult)?
    {
        PlanRevisionDecision::Applied(receipt) | PlanRevisionDecision::Replayed(receipt) => receipt,
    };
    let chain = runtime.plans().verify_revision_chain(plan_id)?;
    println!(
        "[starter-assistant] PLAN_REV2 plan={} revision={} chain_len={} declared_nodes={}",
        short_hex(plan_id.as_bytes()),
        plan_rev2_receipt.revision,
        chain.revision_count,
        plan_rev2_receipt.declared_node_count,
    );

    // 7. Execute newly added Node 3 (`verify-brief`) -> charge LLM tokens -> publish Artifact revision 3.
    let (verify_node_id, verify_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_VERIFY_BRIEF)?;
    materialize_and_activate_node(
        &runtime,
        plan_id,
        verify_node_id,
        verify_rev,
        b"node3-verify",
    )?;
    let llm_receipt_3 = charge_step_tokens(
        &resources,
        &runtime,
        account.account_id,
        b"llm-step3",
        430,
        210,
        80,
        2_600,
    )?;
    let rev3_ms = runtime.wall_now_ms(app_key(b"brief-rev3", 0))?;
    let rev3_body = format!(
        "{}\n## Verification Audit (Revision 3 - v1.1.0)\n\
         - upstream_revision_digest: {}\n\
         - verifier_node: {}\n\
         - llm_receipt: {}\n\
         - status: VERIFIED\n",
        String::from_utf8_lossy(&rev2_readback).trim_end(),
        hex_encode(rev2_digest.as_bytes()),
        hex_encode(verify_node_id.as_bytes()),
        hex_encode(llm_receipt_3.receipt_id.as_bytes()),
    );
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id: brief_id,
        expected_head_revision: 2,
        bytes: rev3_body.as_bytes(),
        created_at_ms: rev3_ms,
        provenance: ProvenanceSourceTriple {
            source_a: *verify_node_id.as_bytes(),
            source_b: *llm_receipt_3.receipt_id.as_bytes(),
            source_digest: rev2_digest,
        },
    })?;
    complete_active_node(
        &runtime,
        plan_id,
        verify_node_id,
        verify_rev,
        b"node3-verify",
    )?;
    let (_, rev3_digest) =
        verify_and_read_artifact_revision(&runtime.artifacts, brief_id, 3, rev3_ms)?;

    let final_budget = resources.inspect_llm_token_budget(account.account_id)?;
    let llm_receipts = resources.list_llm_token_consumptions(account.account_id)?;
    let metrics_preview = final_budget.to_open_metrics_text();
    if !metrics_preview.contains("nlos_llm_budget_consumed_api_calls_total 3") {
        return Err(AppError::Failed(
            "unexpected OpenMetrics LLM token budget exposition".to_string(),
        ));
    }

    // 8. Clean uninstall and post-uninstall fail-closed verification.
    let uninstalled_at_ms = runtime.wall_now_ms(app_key(b"uninstall", 0))?;
    let uninstall_receipt =
        match runtime
            .applications
            .uninstall_application(UninstallApplicationRequest {
                package_id,
                idempotency_key: app_key(b"uninstall", 1),
                uninstalled_at_ms,
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
        "[starter-assistant] UNINSTALL application={} generation={} status=uninstalled remaining_tokens={} remaining_calls={}",
        short_hex(application_id.as_bytes()),
        uninstall_receipt.application_generation.get(),
        final_budget.remaining_total_tokens(),
        final_budget.remaining_api_calls(),
    );

    Ok(WorkflowSummary {
        package_id,
        application_id,
        plan_id,
        brief_artifact_id: brief_id,
        account_id: account.account_id,
        v1_verification_receipt: verify_v1.receipt_id,
        v2_verification_receipt: verify_v2.receipt_id,
        v1_payload_digest: payload_v1.payload_digest,
        v2_payload_digest: payload_v2.payload_digest,
        llm_receipts,
        final_budget,
        final_brief_digest: rev3_digest,
        uninstall_generation: uninstall_receipt.application_generation.get(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nlos_application::{
        ApplicationAuthorityError, CompatibilityWindow, UpdateApplicationRequest,
    };
    use nlos_artifact::{HeadState, check_package_file};
    use nlos_plan::{
        ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanNodeState, PlanStoreError,
    };
    use nlos_resource::{
        ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest,
        LlmBudgetDimension, LlmTokenConsumeDecision, LlmTokenLimits, ResourceAuthority,
        ResourceAuthorityError,
    };
    use nlos_slice_k::SliceKRuntime;
    use nlos_types::Generation;

    use super::{
        NODE_KEY_DRAFT_PLAN, NODE_KEY_SYNTHESIZE_ARTIFACT, NODE_KEY_VERIFY_BRIEF, SEED_INSTALL_V1,
        TaskDeclarationConsultAdapter, TempStateDir, app_key, app_operation_id,
        build_and_check_package_tree, materialize_and_activate_node,
        materialize_and_verify_package, parse_root_arg, resolve_node_id_by_key,
        run_productivity_workflow,
    };

    #[test]
    fn cli_argument_parser_accepts_default_and_explicit_root_only() {
        assert_eq!(parse_root_arg(&[]).expect("empty args"), None);
        let explicit =
            parse_root_arg(&["--root".to_string(), "trial-state".to_string()]).expect("valid root");
        assert_eq!(explicit.as_deref(), Some(Path::new("trial-state")));
        assert!(parse_root_arg(&["--unknown".to_string()]).is_err());
        assert!(parse_root_arg(&["--root".to_string()]).is_err());
        assert!(parse_root_arg(&["--root".to_string(), "--bad".to_string()]).is_err());
    }

    #[test]
    fn starter_package_trees_pass_conformance_and_detect_tampering() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let v1 = build_and_check_package_tree(&manifest_dir.join("package")).expect("build v1.0.0");
        let v2 = build_and_check_package_tree(&manifest_dir.join("package-v1.1.0"))
            .expect("build v1.1.0");

        assert_eq!(v1.decoded.package_id, v2.decoded.package_id);
        assert_ne!(v1.decoded.version, v2.decoded.version);
        assert_eq!(v1.decoded.entries.len(), 2);
        assert_eq!(v1.decoded.tasks.len(), 2);
        assert_eq!(v2.decoded.tasks.len(), 3);

        // Determinism: rebuilding produces byte-identical `.nlospkg` framing.
        let v1_again =
            build_and_check_package_tree(&manifest_dir.join("package")).expect("rebuild v1.0.0");
        assert_eq!(v1.encoded_bytes, v1_again.encoded_bytes);

        // Tampering a payload byte inside the encoded package fails conformance (`PKG-CONF-030`).
        let mut tampered = v1.encoded_bytes.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xFF;
        assert!(!check_package_file(&tampered).is_conformant());
    }

    #[test]
    fn end_to_end_productivity_workflow_commits_all_receipts_and_uninstalls_cleanly() {
        let temp = TempStateDir::new("e2e").expect("temp dir");
        let summary = run_productivity_workflow(&temp.path).expect("run full starter workflow");

        assert_ne!(
            summary.v1_verification_receipt,
            summary.v2_verification_receipt
        );
        assert_ne!(summary.v1_payload_digest, summary.v2_payload_digest);
        assert_eq!(summary.uninstall_generation, 2);

        // Verify five-dimension LLM token budget accounting across the 3 DAG steps:
        // Step 1: 540 + 280 + 120 = 940 tokens, 3_200 uc
        // Step 2: 890 + 610 + 190 = 1_690 tokens, 5_400 uc
        // Step 3: 430 + 210 + 80  = 720 tokens, 2_600 uc
        // Total : 1_860 prompt, 1_100 completion, 390 reasoning, 3_350 total, 3 calls, 11_200 uc
        assert_eq!(summary.llm_receipts.len(), 3);
        assert_eq!(summary.final_budget.consumed_prompt_tokens, 1_860);
        assert_eq!(summary.final_budget.consumed_completion_tokens, 1_100);
        assert_eq!(summary.final_budget.consumed_reasoning_tokens, 390);
        assert_eq!(summary.final_budget.consumed_total_tokens, 3_350);
        assert_eq!(summary.final_budget.consumed_api_calls, 3);
        assert_eq!(summary.final_budget.consumed_cost_microcredits, 11_200);
        assert_eq!(summary.final_budget.remaining_api_calls(), 5);

        // Reopen authorities independently to verify durable persistence on disk.
        let runtime = SliceKRuntime::open(&temp.path).expect("reopen SliceKRuntime");
        let head: HeadState = runtime
            .artifacts
            .resolve_head(summary.brief_artifact_id, u64::MAX)
            .expect("resolve brief head")
            .expect("brief head present");
        assert_eq!(head.revision, 3);
        assert_eq!(head.digest, summary.final_brief_digest);

        for node_hex in [
            NODE_KEY_DRAFT_PLAN,
            NODE_KEY_SYNTHESIZE_ARTIFACT,
            NODE_KEY_VERIFY_BRIEF,
        ] {
            let (node_id, _) =
                resolve_node_id_by_key(runtime.plans(), summary.plan_id, node_hex).expect("node");
            let record = runtime
                .plans()
                .inspect_node(summary.plan_id, node_id)
                .expect("inspect node")
                .expect("node exists");
            assert_eq!(record.state, PlanNodeState::Completed);
        }
    }

    #[test]
    fn llm_token_budget_idempotent_replay_and_exhaustion_fail_closed() {
        let temp = TempStateDir::new("neg-budget").expect("temp dir");
        let resources =
            ResourceAuthority::open(temp.path.join("resources")).expect("open resources");

        let account = resources
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: app_key(b"neg-budget", 0),
                created_at_ms: 100,
            })
            .expect("create account");
        resources
            .create_llm_token_budget(CreateLlmTokenBudgetRequest {
                account_id: account.account_id,
                limits: LlmTokenLimits {
                    max_prompt_tokens: 500,
                    max_completion_tokens: 500,
                    max_total_tokens: 800,
                    max_api_calls: 2,
                    max_cost_microcredits: 5_000,
                },
                idempotency_key: app_key(b"neg-budget", 1),
                created_at_ms: 100,
            })
            .expect("create tight budget");

        let req = ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: app_operation_id(b"neg-budget", 2),
            prompt_tokens: 400,
            completion_tokens: 200,
            reasoning_tokens: 50,
            api_calls: 1,
            cost_microcredits: 2_000,
            idempotency_key: app_key(b"neg-budget", 3),
            consumed_at_ms: 110,
        };
        let first = resources.consume_llm_tokens(req).expect("first charge");
        let second = resources.consume_llm_tokens(req).expect("idempotent retry");
        assert!(matches!(second, LlmTokenConsumeDecision::Replayed(_)));
        assert_eq!(first.receipt(), second.receipt());

        let over_limit = resources.consume_llm_tokens(ConsumeLlmTokensRequest {
            account_id: account.account_id,
            operation_id: app_operation_id(b"neg-budget", 4),
            prompt_tokens: 200,
            completion_tokens: 50,
            reasoning_tokens: 0,
            api_calls: 1,
            cost_microcredits: 1_000,
            idempotency_key: app_key(b"neg-budget", 5),
            consumed_at_ms: 120,
        });
        assert!(matches!(
            over_limit,
            Err(ResourceAuthorityError::LlmTokenBudgetExhausted {
                dimension: LlmBudgetDimension::PromptTokens,
                ..
            })
        ));
        let budget_after = resources
            .inspect_llm_token_budget(account.account_id)
            .expect("inspect budget");
        assert_eq!(budget_after.consumed_api_calls, 1);
        assert_eq!(budget_after.consumed_total_tokens, 650);
    }

    #[test]
    fn narrow_compat_window_and_frozen_node_rewrite_fail_closed() {
        let temp = TempStateDir::new("neg-gates").expect("temp dir");
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let built_v1 = build_and_check_package_tree(&manifest_dir.join("package")).expect("v1");
        let built_v2 =
            build_and_check_package_tree(&manifest_dir.join("package-v1.1.0")).expect("v2");

        let runtime = SliceKRuntime::open(&temp.path).expect("open runtime");
        let (_, signed_pkg_v1, verify_v1) =
            materialize_and_verify_package(&runtime, &built_v1, b"neg-v1").expect("verify v1");
        let (_, _, verify_v2) =
            materialize_and_verify_package(&runtime, &built_v2, b"neg-v2").expect("verify v2");
        let installed = runtime
            .install_verified_templated_package(&verify_v1, &signed_pkg_v1, SEED_INSTALL_V1)
            .expect("install v1");

        // 1. SameMinor refusal on v1.0.0 -> v1.1.0 update.
        let narrow_update = runtime.applications.update_application(
            &runtime.artifacts,
            UpdateApplicationRequest {
                package_id: installed.installation.package_id,
                package_verification_receipt_id: verify_v2.receipt_id,
                idempotency_key: app_key(b"neg-update", 0),
                updated_at_ms: runtime
                    .wall_now_ms(app_key(b"neg-update", 1))
                    .expect("clock"),
                compatibility_window: CompatibilityWindow::SameMinor,
            },
        );
        assert!(matches!(
            narrow_update,
            Err(ApplicationAuthorityError::UpdateCompatibilityViolation { .. })
        ));
        let app_view = runtime
            .applications
            .inspect_application(installed.installation.package_id)
            .expect("inspect app")
            .expect("app exists");
        assert_eq!(
            app_view.current_installation_generation,
            Generation::INITIAL
        );

        // 2. [PLAN-DAG-001] G1 execution-frozen node shape rewrite refusal.
        let plan_id = installed.plan_receipt().plan_id;
        let (draft_id, draft_rev) =
            resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_DRAFT_PLAN).expect("draft");
        materialize_and_activate_node(&runtime, plan_id, draft_id, draft_rev, b"neg-freeze")
            .expect("cross execution boundary");
        let consult = TaskDeclarationConsultAdapter {
            tasks: &runtime.tasks,
        };
        let illegal_reshape = runtime.plans().apply_plan_revision_with_admission(
            ApplyPlanRevisionRequest {
                plan_id: Some(plan_id),
                nodes: vec![PlanNodeDeclaration {
                    node_key: signed_pkg_v1.tasks[0].node_key,
                    kind: PlanNodeKind::AgentRole,
                    binding_digest: [0xEE; 32],
                    dependency_keys: Vec::new(),
                    input_selectors_digest: signed_pkg_v1.tasks[0].input_selectors_digest,
                    output_contract_digest: signed_pkg_v1.tasks[0].output_contract_digest,
                    policy_digest: signed_pkg_v1.tasks[0].policy_digest,
                    resource_ceiling_digest: signed_pkg_v1.tasks[0].resource_ceiling_digest,
                    conditions: None,
                }],
                idempotency_key: app_key(b"neg-reshape", 0),
                applied_at_ms: runtime
                    .wall_now_ms(app_key(b"neg-reshape", 1))
                    .expect("clock"),
            },
            &consult,
        );
        assert!(matches!(
            illegal_reshape,
            Err(PlanStoreError::FrozenNodeShapeRewrite { .. })
        ));
    }
}
