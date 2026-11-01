//! `field-survey-app` — W80-E6B third-party multi-component reference
//! Application (`examples/ecosystem-apps`, Stage E `E6`): a simulated
//! independent publisher ("fieldkit-labs") running the field-survey loop
//! `plan-route -> collect-notes -> sync-notes` (v1.1.0 appends
//! `review-notes`) over public NLOS authorities only, mirroring the
//! `knowledge-observatory-app` shape with a distinct package identity, DAG,
//! budgets, and negative-gate coverage (`SameMinor` refusal and frozen-node
//! rewrite refusal).

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

const USAGE: &str = "usage: field-survey-app [--root <DIR>]";

const EXECUTABLE_ENTRY: &str = "survey-worker";
const SERVICE_ENTRY: &str = "sync-field-notes";
const CHECKLIST_ENTRY: &str = "survey-checklist";
const ROUTE_ENTRY: &str = "route-profile";

const DEV_SIGNER_SEED: [u8; 32] = [0xF1; 32];
const SEED_INSTALL_V1: u8 = 0xE5;

const NODE_KEY_PLAN_ROUTE: &str = "51a1c2d3e4f5061728394a5b6c7d8e01";
const NODE_KEY_COLLECT: &str = "52b2c3d4e5f60718293a4b5c6d7e8f02";
const NODE_KEY_SYNC: &str = "53c3d4e5f60718293a4b5c6d7e8f0303";
const NODE_KEY_REVIEW: &str = "54d4e5f60718293a4b5c6d7e8f030404";

const APP_KEY_DOMAIN: &[u8] = b"llmos/field-survey-app/key/v1";
const LOG_ARTIFACT_DOMAIN: &[u8] = b"llmos/field-survey-app/log-artifact/v1";
const SIGNER_PROFILE_DOMAIN: &[u8] = b"llmos/field-survey-app/signer-profile/v1";
const SIGNER_POLICY_DOMAIN: &[u8] = b"llmos/field-survey-app/signer-policy/v1";
const SIGNER_BOOTSTRAP_DOMAIN: &[u8] = b"llmos/field-survey-app/signer-bootstrap/v1";

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

impl AppError {
    const fn exit_code(&self) -> u8 {
        match self {
            Self::Usage => 1,
            Self::Failed(_) => 2,
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

#[derive(Debug)]
struct WorkflowSummary {
    package_id: PackageId,
    application_id: ApplicationId,
    plan_id: TaskPlanId,
    log_artifact_id: ArtifactId,
    v2_receipt: ReceiptId,
    llm_steps: usize,
    final_budget: LlmTokenBudgetRecord,
    uninstall_generation: u64,
}

struct TempStateDir {
    path: PathBuf,
}

impl TempStateDir {
    fn new(tag: &str) -> AppResult<Self> {
        let seq = NEXT_TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "field-survey-app-{tag}-{}-{seq}",
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
        Ok(value) => value,
        Err(error) => {
            eprintln!("field-survey-app: {error}");
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
    match run_field_survey_workflow(state_root) {
        Ok(summary) => {
            println!(
                "[field-survey] SUMMARY package={} application={} plan={} log_artifact={} v2_receipt={} llm_steps={} consumed_tokens={} uninstall_generation={}",
                short_hex(summary.package_id.as_bytes()),
                short_hex(summary.application_id.as_bytes()),
                short_hex(summary.plan_id.as_bytes()),
                short_hex(summary.log_artifact_id.as_bytes()),
                short_hex(summary.v2_receipt.as_bytes()),
                summary.llm_steps,
                summary.final_budget.consumed_total_tokens,
                summary.uninstall_generation,
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("field-survey-app: {error}");
            ExitCode::from(2)
        }
    }
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

fn log_artifact_id(application_id: ApplicationId, plan_id: TaskPlanId) -> ArtifactId {
    let digest = Sha256::new()
        .chain_update(LOG_ARTIFACT_DOMAIN)
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
        match key.trim() {
            "package-id" => {
                package_id = Some(PackageId::from_bytes(parse_hex16(
                    value.trim(),
                    "package-id",
                )?));
            }
            "version" => {
                version = Some(parse_dotted_version(value.trim())?);
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
                    other => return Err(AppError::Failed(format!("unknown entry role {other:?}"))),
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
    let package_file = PackageFile {
        descriptor,
        package_id: parsed.package_id,
        version: parsed.version,
        entries,
        tasks,
        signature: signing_key.sign(&signing_message).to_bytes(),
    };
    let encoded_bytes = package_file.encode();
    if !check_package_file(&encoded_bytes).is_conformant() {
        return Err(AppError::Failed(format!(
            "package conformance check failed for {}",
            tree.display()
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
fn run_field_survey_workflow(state_root: &Path) -> AppResult<WorkflowSummary> {
    fs::create_dir_all(state_root)?;
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let built_v1 = build_and_check_package_tree(&manifest_dir.join("package"))?;
    let built_v2 = build_and_check_package_tree(&manifest_dir.join("package-v1.1.0"))?;
    println!(
        "[field-survey] PACKAGES_CONFORMANT v1_entries={} v1_tasks={} v1_bytes={} v2_tasks={} v2_bytes={}",
        built_v1.decoded.entries.len(),
        built_v1.decoded.tasks.len(),
        built_v1.encoded_bytes.len(),
        built_v2.decoded.tasks.len(),
        built_v2.encoded_bytes.len(),
    );

    let runtime = SliceKRuntime::open(state_root)?;
    let resources = ResourceAuthority::open(state_root.join("resources"))?;

    let (_principal_v1, signed_pkg_v1, verify_v1) =
        materialize_and_verify_package(&runtime, &built_v1, b"verify-v1")?;
    let templated_install =
        runtime.install_verified_templated_package(&verify_v1, &signed_pkg_v1, SEED_INSTALL_V1)?;
    let installation_v1 = templated_install.installation.clone();
    let plan_id = templated_install.plan_receipt().plan_id;
    let package_id = installation_v1.package_id;
    let application_id = installation_v1.application_id;

    let payload_v1 = execute_application_payload(&runtime, package_id, EXECUTABLE_ENTRY)?;
    println!(
        "[field-survey] INSTALL_V1 generation={} plan_nodes={} payload_entry={} payload_digest={}",
        installation_v1.installation_generation.get(),
        templated_install.plan_receipt().declared_node_count,
        payload_v1.entry_name,
        short_hex(payload_v1.payload_digest.as_bytes()),
    );

    // Read the background-service payload and both read-only seed entries back.
    let seed_at = runtime.wall_now_ms(app_key(b"seed-read", 0))?;
    let _service_bytes = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        SERVICE_ENTRY,
        seed_at,
    )?;
    let checklist = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        CHECKLIST_ENTRY,
        seed_at,
    )?;
    let route_profile = read_seed_entry(
        &runtime.artifacts,
        package_id,
        installation_v1.package_version,
        ROUTE_ENTRY,
        seed_at,
    )?;
    let mut upstream_digest = ContentDigest::of_bytes(&checklist);
    let _ = route_profile.len();

    let budget_at = runtime.wall_now_ms(app_key(b"budget", 0))?;
    let account = resources.create_account(CreateAccountRequest {
        initial_credit: 120_000,
        idempotency_key: app_key(b"budget", 1),
        created_at_ms: budget_at,
    })?;
    resources.create_llm_token_budget(CreateLlmTokenBudgetRequest {
        account_id: account.account_id,
        limits: LlmTokenLimits {
            max_prompt_tokens: 8_192,
            max_completion_tokens: 4_096,
            max_total_tokens: 14_336,
            max_api_calls: 9,
            max_cost_microcredits: 60_000,
        },
        idempotency_key: app_key(b"budget", 2),
        created_at_ms: budget_at,
    })?;

    // Dependency gate: collect before plan-route must fail closed.
    let (route_node, route_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_PLAN_ROUTE)?;
    let (collect_node, collect_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_COLLECT)?;
    let (sync_node, sync_rev) = resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_SYNC)?;
    let premature_at = runtime.wall_now_ms(app_key(b"premature", 0))?;
    match runtime
        .plans()
        .request_materialization(MaterializationRequest {
            plan_id,
            node_id: collect_node,
            idempotency_key: app_key(b"premature", 1),
            requested_at_ms: premature_at,
        }) {
        Err(PlanStoreError::DependenciesNotReady { unresolved, .. }) => {
            println!(
                "[field-survey] PLAN_GATE_BLOCKED node=collect unresolved_deps={}",
                unresolved.len(),
            );
        }
        Err(other) => return Err(other.into()),
        Ok(_) => {
            return Err(AppError::Failed(
                "collect-notes unexpectedly materialized before plan-route completed".to_string(),
            ));
        }
    }

    let log_id = log_artifact_id(application_id, plan_id);
    let mut llm_steps = 0_usize;
    let steps = [
        (
            "plan-route",
            route_node,
            route_rev,
            "route",
            420_u64,
            130,
            30,
            1_500_u64,
        ),
        (
            "collect-notes",
            collect_node,
            collect_rev,
            "collect",
            760,
            340,
            150,
            3_900,
        ),
        (
            "sync-notes",
            sync_node,
            sync_rev,
            "sync",
            320,
            140,
            60,
            1_700,
        ),
    ];
    let mut revision = 0_u64;
    for (label, node_id, declared_revision, phase, prompt, completion, reasoning, cost) in steps {
        let phase = phase.as_bytes();
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
        llm_steps += 1;
        revision += 1;
        let at_ms = runtime.wall_now_ms(app_key(phase, 20))?;
        let body = format!(
            "# Field Survey Log (Revision {revision} - {label})\n\
             - plan: {}\n\
             - llm_receipt: {}\n\
             - upstream_digest: {}\n",
            hex_encode(plan_id.as_bytes()),
            hex_encode(receipt.receipt_id.as_bytes()),
            hex_encode(upstream_digest.as_bytes()),
        );
        if revision == 1 {
            runtime.artifacts.create_artifact(CreateArtifactSpec {
                artifact_id: log_id,
                idempotency_key: app_key(phase, 21),
                content_type: "text/markdown".to_string(),
                application_id: Some(application_id),
                owner: None,
                created_at_ms: at_ms,
            })?;
        }
        runtime.artifacts.put_revision(PutRevisionRequest {
            artifact_id: log_id,
            expected_head_revision: revision - 1,
            bytes: body.as_bytes(),
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: *node_id.as_bytes(),
                source_b: *receipt.receipt_id.as_bytes(),
                source_digest: upstream_digest,
            },
        })?;
        complete_active_node(&runtime, plan_id, node_id, declared_revision, phase)?;
        upstream_digest = ContentDigest::of_bytes(body.as_bytes());
    }

    // SameMajor upgrade + plan revision 2 with appended review-notes node.
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
    let plan_rev2_at = runtime.wall_now_ms(app_key(b"plan-rev2", 0))?;
    let plan_rev2_key =
        install_plan_revision_key(application_id, installation_v2.installation_generation);
    let consult = TaskDeclarationConsultAdapter {
        tasks: &runtime.tasks,
    };
    let mut plan_rev2_req = compile_task_templates(&signed_pkg_v2, plan_rev2_key, plan_rev2_at)
        .map_err(|error| AppError::Failed(format!("compile v1.1.0 task templates: {error:?}")))?;
    plan_rev2_req.plan_id = Some(plan_id);
    let plan_rev2 = match runtime
        .plans()
        .apply_plan_revision_with_admission(plan_rev2_req, &consult)?
    {
        PlanRevisionDecision::Applied(receipt) | PlanRevisionDecision::Replayed(receipt) => receipt,
    };
    println!(
        "[field-survey] UPDATE_V1_1 generation={} plan_rev={} nodes={}",
        installation_v2.installation_generation.get(),
        plan_rev2.revision,
        plan_rev2.declared_node_count,
    );

    let (review_node, review_rev) =
        resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_REVIEW)?;
    materialize_and_activate_node(&runtime, plan_id, review_node, review_rev, b"step-review")?;
    let review_receipt = charge_step_tokens(
        &resources,
        &runtime,
        account.account_id,
        b"step-review",
        260,
        110,
        50,
        1_200,
    )?;
    llm_steps += 1;
    let rev4_at = runtime.wall_now_ms(app_key(b"step-review", 22))?;
    let rev4_body = format!(
        "# Field Survey Log (Revision 4 - review)\n\
         - upstream_digest: {}\n\
         - status: REVIEWED\n",
        hex_encode(upstream_digest.as_bytes()),
    );
    runtime.artifacts.put_revision(PutRevisionRequest {
        artifact_id: log_id,
        expected_head_revision: 3,
        bytes: rev4_body.as_bytes(),
        created_at_ms: rev4_at,
        provenance: ProvenanceSourceTriple {
            source_a: *review_node.as_bytes(),
            source_b: *review_receipt.receipt_id.as_bytes(),
            source_digest: upstream_digest,
        },
    })?;
    complete_active_node(&runtime, plan_id, review_node, review_rev, b"step-review")?;

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

    Ok(WorkflowSummary {
        package_id,
        application_id,
        plan_id,
        log_artifact_id: log_id,
        v2_receipt: verify_v2.receipt_id,
        llm_steps,
        final_budget,
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
    let decision = runtime
        .plans()
        .resolve_materialization(MaterializationResolution {
            request_key,
            verdict: MaterializationAdmissionVerdict::Approved(MaterializationAdmission {
                profile_id: "fieldkit-local-workstation".to_string(),
                projected_task_nodes: projected_nodes,
                projected_active_working_set: 1,
            }),
            resolved_at_ms: runtime.wall_now_ms(app_key(phase, 3))?,
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
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Materializing,
            to_state: PlanNodeState::Active,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 5),
            transitioned_at_ms: runtime.wall_now_ms(app_key(phase, 4))?,
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
    let _ = runtime
        .plans()
        .record_node_transition(NodeTransitionRequest {
            plan_id,
            node_id,
            from_state: PlanNodeState::Active,
            to_state: PlanNodeState::Completed,
            expected_declared_revision: declared_revision,
            idempotency_key: app_key(phase, 7),
            transitioned_at_ms: runtime.wall_now_ms(app_key(phase, 6))?,
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
    Ok(resources
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
        .receipt())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use nlos_application::{
        ApplicationAuthorityError, CompatibilityWindow, UpdateApplicationRequest,
    };
    use nlos_artifact::{HeadState, check_package_file};
    use nlos_plan::{ApplyPlanRevisionRequest, PlanNodeDeclaration, PlanNodeKind, PlanStoreError};
    use nlos_slice_k::SliceKRuntime;
    use nlos_types::Generation;

    use super::{
        NODE_KEY_COLLECT, NODE_KEY_PLAN_ROUTE, NODE_KEY_REVIEW, NODE_KEY_SYNC, SEED_INSTALL_V1,
        SERVICE_ENTRY, TaskDeclarationConsultAdapter, TempStateDir, app_key,
        build_and_check_package_tree, materialize_and_activate_node,
        materialize_and_verify_package, parse_root_arg, resolve_node_id_by_key,
        run_field_survey_workflow,
    };

    #[test]
    fn cli_argument_parser_accepts_default_and_explicit_root_only() {
        assert_eq!(parse_root_arg(&[]).expect("empty args"), None);
        let explicit = parse_root_arg(&["--root".to_string(), "state".to_string()]).expect("root");
        assert_eq!(explicit.as_deref(), Some(Path::new("state")));
        assert!(parse_root_arg(&["extra".to_string()]).is_err());
    }

    #[test]
    fn multi_component_trees_pass_conformance_and_determinism() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let v1 = build_and_check_package_tree(&manifest_dir.join("package")).expect("build v1.0.0");
        let v2 = build_and_check_package_tree(&manifest_dir.join("package-v1.1.0"))
            .expect("build v1.1.0");
        assert_eq!(v1.decoded.package_id, v2.decoded.package_id);
        assert_ne!(v1.decoded.version, v2.decoded.version);
        assert_eq!(v1.decoded.entries.len(), 6);
        assert_eq!(v1.decoded.tasks.len(), 3);
        assert_eq!(v2.decoded.tasks.len(), 4);
        assert!(
            v1.decoded
                .entries
                .iter()
                .any(|entry| entry.name == SERVICE_ENTRY)
        );
        let rebuilt = build_and_check_package_tree(&manifest_dir.join("package")).expect("rebuild");
        assert_eq!(v1.encoded_bytes, rebuilt.encoded_bytes);

        let mut tampered = v2.encoded_bytes.clone();
        let mid = tampered.len() / 2;
        tampered[mid] ^= 0xFF;
        assert!(!check_package_file(&tampered).is_conformant());
    }

    #[test]
    fn end_to_end_workflow_upgrades_and_uninstalls_cleanly() {
        let temp = TempStateDir::new("e2e").expect("temp dir");
        let summary = run_field_survey_workflow(&temp.path).expect("run field survey workflow");
        assert_eq!(summary.uninstall_generation, 2);
        assert_eq!(summary.llm_steps, 4);
        // 1_760 prompt + 720 completion + 290 reasoning = 2_770 total, 8_300 uc.
        assert_eq!(summary.final_budget.consumed_prompt_tokens, 1_760);
        assert_eq!(summary.final_budget.consumed_completion_tokens, 720);
        assert_eq!(summary.final_budget.consumed_reasoning_tokens, 290);
        assert_eq!(summary.final_budget.consumed_total_tokens, 2_770);
        assert_eq!(summary.final_budget.consumed_cost_microcredits, 8_300);
        assert_eq!(summary.final_budget.remaining_api_calls(), 5);

        let runtime = SliceKRuntime::open(&temp.path).expect("reopen");
        let head: HeadState = runtime
            .artifacts
            .resolve_head(summary.log_artifact_id, u64::MAX)
            .expect("resolve head")
            .expect("head present");
        assert_eq!(head.revision, 4);
        for node_hex in [
            NODE_KEY_PLAN_ROUTE,
            NODE_KEY_COLLECT,
            NODE_KEY_SYNC,
            NODE_KEY_REVIEW,
        ] {
            let (node_id, _) =
                resolve_node_id_by_key(runtime.plans(), summary.plan_id, node_hex).expect("node");
            let record = runtime
                .plans()
                .inspect_node(summary.plan_id, node_id)
                .expect("inspect")
                .expect("node");
            assert_eq!(record.state, nlos_plan::PlanNodeState::Completed);
        }
    }

    #[test]
    fn narrow_window_and_frozen_node_rewrite_fail_closed() {
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

        // 1. SameMinor refusal on v1.0.0 -> v1.1.0.
        let narrow = runtime.applications.update_application(
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
            narrow,
            Err(ApplicationAuthorityError::UpdateCompatibilityViolation { .. })
        ));
        let app_view = runtime
            .applications
            .inspect_application(installed.installation.package_id)
            .expect("inspect")
            .expect("app");
        assert_eq!(
            app_view.current_installation_generation,
            Generation::INITIAL
        );

        // 2. [PLAN-DAG-001] G1 execution-frozen node shape rewrite refusal.
        let plan_id = installed.plan_receipt().plan_id;
        let (route_id, route_rev) =
            resolve_node_id_by_key(runtime.plans(), plan_id, NODE_KEY_PLAN_ROUTE).expect("route");
        materialize_and_activate_node(&runtime, plan_id, route_id, route_rev, b"neg-freeze")
            .expect("cross execution boundary");
        let consult = TaskDeclarationConsultAdapter {
            tasks: &runtime.tasks,
        };
        let illegal = runtime.plans().apply_plan_revision_with_admission(
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
            illegal,
            Err(PlanStoreError::FrozenNodeShapeRewrite { .. })
        ));
    }
}
