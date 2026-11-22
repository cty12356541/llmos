//! `[ROAD-A-003]` / `[NLOS-GEN-001]` pure-code, zero-model, network-isolated
//! end-to-end longitudinal proof:
//!
//! 1. **Pure-code manifest & install**: builds a package declaring only
//!    `PackageTaskKind::Executable` (zero `AgentRole` nodes, zero LLM/HTTP
//!    provider drivers), signs it with Ed25519, verifies via
//!    `verify_package_with_tasks`, checks `compile_task_templates`, and installs
//!    at generation 1 (`ApplicationStatus::Installed`).
//! 2. **Fast zero-model execution -> user `Artifact` Revision 1**: spawns an
//!    isolated OS worker subprocess via `std::env::current_exe()` with
//!    `.env_clear()`, audits its environment to prove zero LLM API keys or
//!    proxy variables, runs deterministic pure-code computation with SHA-256
//!    self-attestation, registers the live OS PID in `SupervisorPidRegistry`,
//!    and commits Revision 1 through `Task`/`Attempt` + `CommitPermit` +
//!    staging + `WriteFiberJob` + `TaskCommitReceipt` (no model driver path).
//! 3. **Real `SIGKILL` crash injection & lossless recovery -> Revision 2**:
//!    spawns a second pure-code OS worker that halts at a pre-write barrier,
//!    kills it from the parent (`child.kill()` + `child.wait()`), verifies the
//!    head stays durably at Revision 1 and a failed/cancelled Attempt never
//!    mutates the authoritative head, then runs a clean recovery Attempt with a
//!    third pure-code OS worker to commit Revision 2 + `TaskCommitReceipt`.
//! 4. **Activity-gated zero-residue teardown + `PKG-UNINSTALL-001` `UserData`
//!    preservation**: confirms gated uninstall refuses while the background
//!    Task is active, runs `run_application_teardown` (terminating the live OS
//!    worker on Unix, canceling the Task, uninstalling, and replaying
//!    byte-identically), runs `collect_orphan_blobs` + runtime reopen, and
//!    verifies both Revision 1 and Revision 2 of the user `Artifact` remain
//!    readable with intact bytes and SHA-256 digests.
//! 5. **Negative paths**: tampered pure-code manifest signature failure with
//!    zero durable authority writes, pre-teardown uninstall refusal, empty
//!    supervisor PID refusal on Unix teardown, and two-hop `.env_clear()`
//!    isolation proof against an intentionally poisoned parent environment.

use std::collections::BTreeSet;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::Signer;
use nlos_application::{
    ApplicationAuthorityError, ApplicationStatus, compile_task_templates, pack_package_version,
};
use nlos_artifact::{
    ArtifactError, CollectOrphanBlobsDecision, ContentDigest, CreateArtifactSpec, PackageEntryRole,
    PackageManifest, PackageManifestEntry, PackageTaskKind, PackageTaskTemplate,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackageWithTasks,
    VerifyPackageWithTasksRequest, derive_artifact_id, package_manifest_with_tasks_message,
    staging_id_for,
};
use nlos_plan::PlanNodeKind;
#[cfg(unix)]
use nlos_process::ProcessAuthorityError;
use nlos_process::{
    PlatformKillDecision, ProcessLifecycleState, PropagateCrashRequest,
    RegisterSupervisorPidRequest, SupervisorPidRegistry,
};
use nlos_runtime::{FiberSpec, FiberState, RuntimeAdapter as _, RuntimeError};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    SliceKError, SliceKRuntime, WriteFiberJob, plant_orphan_artifact_blob,
    run_application_teardown, spawn_write_fiber,
};
use nlos_task::{
    ArtifactPublicationExpectation, AttemptSpec, AttemptState, Authorities, CancelDecision,
    CancelRequest, PermitDecision, PermitRequest, PermitState, SnapshotBundle, TaskState,
    artifact_publication_plan_root,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation,
    IdempotencyKey, OperationId, PackageId, ReceiptId, ResourceGroupId, SchedulerDomainId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use sha2::{Digest, Sha256};

const WORKER_TEST_NAME: &str = "road_a003_pure_code_child_worker";
const ENV_WORKER: &str = "NLOS_PURE_CODE_WORKER";
const ENV_MODE: &str = "NLOS_PURE_CODE_MODE";
const ENV_INPUT: &str = "NLOS_PURE_CODE_INPUT";
const ENV_OUTPUT: &str = "NLOS_PURE_CODE_OUTPUT";
const ENV_REPORT: &str = "NLOS_PURE_CODE_REPORT";
const ENV_READY: &str = "NLOS_PURE_CODE_READY";
const ENV_STOP: &str = "NLOS_PURE_CODE_STOP";
const ENV_GRANDCHILD_REPORT: &str = "NLOS_PURE_CODE_GRANDCHILD_REPORT";
const ENV_GRANDCHILD_READY: &str = "NLOS_PURE_CODE_GRANDCHILD_READY";

const KEY_DOMAIN: &[u8] = b"llmos/road-a003-pure-code/key/v1";
const ID_DOMAIN: &[u8] = b"llmos/road-a003-pure-code/id/v1";
const CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(20);
const CHILD_HOLD_CAP: Duration = Duration::from_secs(30);

const FORBIDDEN_MODEL_AND_PROXY_ENV_KEYS: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GLM_API_KEY",
    "ZHIPU_API_KEY",
    "DEEPSEEK_API_KEY",
    "GEMINI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "LLMOS_KEYPOOL_CONFIG",
    "LLMOS_LLM_BASE_URL",
    "LLMOS_LLM_MODEL",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "NO_PROXY",
];

fn pure_key(label: &str) -> IdempotencyKey {
    let digest = Sha256::new()
        .chain_update(KEY_DOMAIN)
        .chain_update(label.as_bytes())
        .finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

fn pure_id(label: &str) -> [u8; 16] {
    let digest = Sha256::new()
        .chain_update(ID_DOMAIN)
        .chain_update(label.as_bytes())
        .finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes
}

fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    digest.iter().fold(String::with_capacity(64), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio runtime adapter")
}

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-road-a003-{label}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => panic!("remove pure-code temp root: {err}"),
        }
    }
}

/// RAII child guard ensuring no spawned OS worker ever leaks on assertion
/// failure or timeout.
struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child still present").id()
    }

    fn try_wait(&mut self) -> Option<ExitStatus> {
        self.child
            .as_mut()
            .expect("child still present")
            .try_wait()
            .expect("try_wait child")
    }

    fn await_ready_file(&mut self, ready_path: &Path, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        loop {
            if ready_path.is_file() {
                return;
            }
            if let Some(status) = self.try_wait() {
                assert!(
                    ready_path.is_file(),
                    "pure-code child exited prematurely with {status} before writing {}",
                    ready_path.display()
                );
                return;
            }
            if Instant::now() >= deadline {
                let status = self.kill_and_reap();
                panic!(
                    "timed out waiting for READY flag {}; killed child ({status})",
                    ready_path.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_with_timeout(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.try_wait() {
                self.child = None;
                return status;
            }
            if Instant::now() >= deadline {
                let status = self.kill_and_reap();
                panic!("pure-code child timed out after {timeout:?}; killed ({status})");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_reap(&mut self) -> ExitStatus {
        let mut child = self.child.take().expect("child still present");
        let _ = child.kill();
        child.wait().expect("reap killed child")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take()
            && matches!(child.try_wait(), Ok(None))
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn write_file_atomic(path: &Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent dir");
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, bytes).expect("write temp file");
    fs::rename(&tmp, path).expect("atomic rename file");
}

fn spawn_isolated_pure_code_worker(
    mode: &str,
    input: Option<&str>,
    output_path: Option<&Path>,
    report_path: &Path,
    ready_path: &Path,
    stop_path: Option<&Path>,
) -> ChildGuard {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", WORKER_TEST_NAME, "--ignored", "--nocapture"])
        .env_clear()
        .env(ENV_WORKER, "1")
        .env(ENV_MODE, mode)
        .env(ENV_REPORT, report_path)
        .env(ENV_READY, ready_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(input_val) = input {
        cmd.env(ENV_INPUT, input_val);
    }
    if let Some(out) = output_path {
        cmd.env(ENV_OUTPUT, out);
    }
    if let Some(stop) = stop_path {
        cmd.env(ENV_STOP, stop);
    }
    ChildGuard::new(cmd.spawn().expect("spawn isolated pure-code child worker"))
}

fn is_forbidden_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    FORBIDDEN_MODEL_AND_PROXY_ENV_KEYS
        .iter()
        .any(|forbidden| upper == *forbidden)
        || upper.contains("API_KEY")
        || upper.contains("PROXY")
        || upper.contains("LLM_")
        || upper.contains("OPENAI")
        || upper.contains("ANTHROPIC")
        || upper.contains("ZHIPU")
        || upper.contains("GLM_")
}

fn audit_child_env_strict() -> (Vec<(String, String)>, String) {
    let mut vars: Vec<(String, String)> = std::env::vars().collect();
    vars.sort_by(|a, b| a.0.cmp(&b.0));

    let forbidden_hits: Vec<&str> = vars
        .iter()
        .filter_map(|(k, _)| is_forbidden_env_key(k).then_some(k.as_str()))
        .collect();
    assert!(
        forbidden_hits.is_empty(),
        "isolated pure-code worker must observe zero model/proxy env vars, found: {forbidden_hits:?}"
    );

    let non_whitelisted: Vec<&str> = vars
        .iter()
        .filter_map(|(k, _)| (!k.starts_with("NLOS_PURE_CODE_")).then_some(k.as_str()))
        .collect();
    assert!(
        non_whitelisted.is_empty(),
        "env_clear pure-code worker observed unexpected inherited env vars: {non_whitelisted:?}"
    );

    let keys_csv = vars
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let entries_joined = vars
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("|");
    (vars, format!("{keys_csv}\nenv_vars={entries_joined}"))
}

fn compute_pure_code_payload(mode: &str, input: &str, worker_pid: u32) -> (Vec<u8>, String) {
    let mut total: i64 = 0;
    let mut normalized_items = Vec::new();
    let mut prev_sha = "none".to_string();

    for segment in input.split(';') {
        if let Some(rest) = segment.strip_prefix("prev_sha256=") {
            prev_sha = rest.to_string();
        } else if let Some(records) = segment.strip_prefix("records=") {
            for pair in records.split(',') {
                let (name, raw_val) = pair
                    .split_once(':')
                    .expect("deterministic record must be key:int");
                let value: i64 = raw_val.parse().expect("record integer value");
                total += value;
                normalized_items.push(format!("{name}={value}"));
            }
        }
    }
    assert!(
        !normalized_items.is_empty(),
        "pure-code worker expects at least one deterministic record"
    );

    let input_sha = sha256_hex(input.as_bytes());
    let payload_text = format!(
        "format=nlos-pure-code-ledger/v1\n\
         mode={mode}\n\
         worker_pid={worker_pid}\n\
         model_provider=none\n\
         network_egress=blocked\n\
         prev_sha256={prev_sha}\n\
         input_sha256={input_sha}\n\
         record_count={}\n\
         checksum_sum={total}\n\
         records={}\n",
        normalized_items.len(),
        normalized_items.join(",")
    );
    let bytes = payload_text.into_bytes();
    let digest_hex = sha256_hex(&bytes);
    (bytes, digest_hex)
}

fn hold_until_stop_or_signal(stop_path: Option<&Path>) {
    let cap = Instant::now() + CHILD_HOLD_CAP;
    while Instant::now() < cap {
        if let Some(stop) = stop_path
            && stop.exists()
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Single `#[ignore]` OS child worker entrypoint invoked by parent tests via
/// `current_exe()` + `["--exact", "road_a003_pure_code_child_worker", "--ignored", "--nocapture"]`
/// after `.env_clear()`.
#[ignore = "pure-code child worker entrypoint invoked by parent E2E tests"]
#[test]
fn road_a003_pure_code_child_worker() {
    if std::env::var_os(ENV_WORKER).is_none() {
        return;
    }

    let mode = std::env::var(ENV_MODE).expect("NLOS_PURE_CODE_MODE");
    let report_path = PathBuf::from(std::env::var(ENV_REPORT).expect("NLOS_PURE_CODE_REPORT"));
    let ready_path = PathBuf::from(std::env::var(ENV_READY).expect("NLOS_PURE_CODE_READY"));
    let pid = std::process::id();

    if mode == "relay-with-env-clear" {
        let observed_keys: BTreeSet<String> = std::env::vars().map(|(k, _)| k).collect();
        let poisoned_present: Vec<&str> = FORBIDDEN_MODEL_AND_PROXY_ENV_KEYS
            .iter()
            .copied()
            .filter(|k| observed_keys.contains(*k))
            .collect();
        assert!(
            poisoned_present.len() >= 7,
            "poisoned-parent control probe expected poisoned keys in own env, saw: {poisoned_present:?}"
        );

        let grandchild_report = PathBuf::from(
            std::env::var(ENV_GRANDCHILD_REPORT).expect("NLOS_PURE_CODE_GRANDCHILD_REPORT"),
        );
        let grandchild_ready = PathBuf::from(
            std::env::var(ENV_GRANDCHILD_READY).expect("NLOS_PURE_CODE_GRANDCHILD_READY"),
        );
        let mut grandchild = spawn_isolated_pure_code_worker(
            "env-audit-only",
            None,
            None,
            &grandchild_report,
            &grandchild_ready,
            None,
        );
        let status = grandchild.wait_with_timeout(CHILD_WAIT_TIMEOUT);
        assert!(
            status.success(),
            "env-audit-only grandchild failed: {status}"
        );

        let relay_report = format!(
            "relay_pid={pid}\npoisoned_keys_seen={}\npoisoned_keys={}\n",
            poisoned_present.len(),
            poisoned_present.join(",")
        );
        write_file_atomic(&report_path, relay_report.as_bytes());
        write_file_atomic(&ready_path, b"RELAY_READY\n");
        println!("RELAY_READY pid={pid}");
        let _ = std::io::stdout().flush();
        return;
    }

    let (vars, keys_csv) = audit_child_env_strict();

    match mode.as_str() {
        "env-audit-only" => {
            let report = format!(
                "worker_pid={pid}\nmode={mode}\nenv_count={}\nenv_keys={keys_csv}\nforbidden_llm_or_proxy_keys=0\n",
                vars.len()
            );
            write_file_atomic(&report_path, report.as_bytes());
            write_file_atomic(&ready_path, b"AUDIT_READY\n");
            println!("AUDIT_READY pid={pid}");
            let _ = std::io::stdout().flush();
        }
        "compute-and-hold" | "recover-compute" => {
            let input = std::env::var(ENV_INPUT).expect("NLOS_PURE_CODE_INPUT");
            let output_path =
                PathBuf::from(std::env::var(ENV_OUTPUT).expect("NLOS_PURE_CODE_OUTPUT"));
            let (output_bytes, output_sha) = compute_pure_code_payload(&mode, &input, pid);
            write_file_atomic(&output_path, &output_bytes);
            let report = format!(
                "worker_pid={pid}\nmode={mode}\nenv_count={}\nenv_keys={keys_csv}\nforbidden_llm_or_proxy_keys=0\noutput_sha256={output_sha}\noutput_bytes={}\n",
                vars.len(),
                output_bytes.len()
            );
            write_file_atomic(&report_path, report.as_bytes());
            write_file_atomic(&ready_path, b"READY\n");
            println!("READY pid={pid} sha256={output_sha}");
            let _ = std::io::stdout().flush();

            if mode == "compute-and-hold" {
                let stop_path = std::env::var_os(ENV_STOP).map(PathBuf::from);
                hold_until_stop_or_signal(stop_path.as_deref());
            }
        }
        "barrier-before-write" => {
            let report = format!(
                "worker_pid={pid}\nmode={mode}\nenv_count={}\nenv_keys={keys_csv}\nforbidden_llm_or_proxy_keys=0\npre_write_barrier=1\n",
                vars.len()
            );
            write_file_atomic(&report_path, report.as_bytes());
            write_file_atomic(&ready_path, b"BARRIER_READY\n");
            println!("BARRIER_READY pid={pid}");
            let _ = std::io::stdout().flush();

            let stop_path = std::env::var_os(ENV_STOP).map(PathBuf::from);
            hold_until_stop_or_signal(stop_path.as_deref());
        }
        other => panic!("unexpected NLOS_PURE_CODE_MODE: {other}"),
    }
}

fn build_pure_code_package(
    runtime: &SliceKRuntime,
    publisher: &nlos_slice_k::Publisher,
    package_id: PackageId,
    version_tag: &str,
) -> (SignedPackageWithTasks, ArtifactId) {
    let version = pack_package_version(1, 0, 0);
    let entry_name = "pure-code-worker";
    let entry_bytes =
        format!("pure-code-executable-manifest-entry {version_tag} model-free=true\n").into_bytes();
    let entry_artifact_id =
        ArtifactId::from_bytes(derive_artifact_id(package_id, version, entry_name));
    let at_ms = runtime
        .wall_now_ms(pure_key(&format!("pkg-publish-clock-{version_tag}")))
        .expect("package publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: entry_artifact_id,
            idempotency_key: pure_key(&format!("pkg-entry-create-{version_tag}")),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .expect("create package executable entry artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id: entry_artifact_id,
            expected_head_revision: 0,
            bytes: &entry_bytes,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: *entry_artifact_id.as_bytes(),
                source_b: *package_id.as_bytes(),
                source_digest: ContentDigest::of_bytes(&entry_bytes),
            },
        })
        .expect("put package executable entry revision");

    let manifest = PackageManifest {
        package_id,
        version,
        entries: vec![PackageManifestEntry {
            name: entry_name.to_string(),
            artifact_id: entry_artifact_id,
            digest: ContentDigest::of_bytes(&entry_bytes),
            role: PackageEntryRole::Executable,
        }],
    };
    let node_compute = pure_id("node-compute");
    let node_persist = pure_id("node-persist");
    let tasks = vec![
        PackageTaskTemplate {
            node_key: node_compute,
            kind: PackageTaskKind::Executable,
            binding_digest: ContentDigest::of_bytes(b"pure-code-compute-worker-v1").into_bytes(),
            dependency_keys: Vec::new(),
            input_selectors_digest: ContentDigest::of_bytes(b"pure-code-inputs").into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"pure-code-ledger-v1").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"pure-code-offline-policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"pure-code-ceiling").into_bytes(),
        },
        PackageTaskTemplate {
            node_key: node_persist,
            kind: PackageTaskKind::Executable,
            binding_digest: ContentDigest::of_bytes(b"pure-code-persist-worker-v1").into_bytes(),
            dependency_keys: vec![node_compute],
            input_selectors_digest: ContentDigest::of_bytes(b"pure-code-persist-inputs")
                .into_bytes(),
            output_contract_digest: ContentDigest::of_bytes(b"pure-code-artifact-rev").into_bytes(),
            policy_digest: ContentDigest::of_bytes(b"pure-code-offline-policy").into_bytes(),
            resource_ceiling_digest: ContentDigest::of_bytes(b"pure-code-ceiling").into_bytes(),
        },
    ];
    let message = package_manifest_with_tasks_message(&manifest, &tasks);
    let signed = SignedPackageWithTasks {
        manifest,
        tasks,
        signer: publisher.principal_id,
        signature: publisher.signing.sign(&message).to_bytes(),
    };
    (signed, entry_artifact_id)
}

fn assert_env_report_clean(report_path: &Path, expected_mode: &str, expected_sha: Option<&str>) {
    let report = fs::read_to_string(report_path).expect("read child audit report");
    assert!(
        report.contains(&format!("mode={expected_mode}")),
        "report missing expected mode={expected_mode}:\n{report}"
    );
    assert!(
        report.contains("forbidden_llm_or_proxy_keys=0"),
        "report missing forbidden_llm_or_proxy_keys=0:\n{report}"
    );
    for forbidden in FORBIDDEN_MODEL_AND_PROXY_ENV_KEYS {
        assert!(
            !report.contains(forbidden),
            "forbidden env var {forbidden} leaked into child report:\n{report}"
        );
    }
    if let Some(sha) = expected_sha {
        assert!(
            report.contains(&format!("output_sha256={sha}")),
            "report missing output_sha256={sha}:\n{report}"
        );
    }
}

struct CommitWriteRequest<'a> {
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    scope_id: CancellationScopeId,
    process: &'a nlos_process::ProcessBindingRecord,
    artifact_id: ArtifactId,
    expected_head_revision: u64,
    bytes: Vec<u8>,
    tag: &'a str,
}

async fn execute_commit_write_fiber(
    runtime: &Arc<SliceKRuntime>,
    tokio_adapter: &TokioRuntimeAdapter,
    req: CommitWriteRequest<'_>,
) -> (
    nlos_task::ArtifactTaskCommitReceipt,
    nlos_types::CommitPermitId,
) {
    let target_revision = req.expected_head_revision + 1;
    let stage_key = pure_key(&format!("{}-stage", req.tag));
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(req.artifact_id, stage_key).into_bytes(),
        artifact_id: req.artifact_id,
        target_revision,
        digest: ContentDigest::of_bytes(&req.bytes).into_bytes(),
        size_bytes: u64::try_from(req.bytes.len()).expect("payload size fits u64"),
    };
    let write_set_root =
        artifact_publication_plan_root(&[expectation]).expect("artifact publication plan root");
    let PermitDecision::Issued(permit) = runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id: req.task_id,
                attempt_id: req.attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: pure_key(&format!("{}-permit", req.tag)),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime
                    .wall_now_i64(pure_key(&format!("{}-permit-clock", req.tag)))
                    .expect("permit clock"),
            },
        )
        .expect("request commit permit")
    else {
        panic!("commit permit must be issued for {}", req.tag);
    };

    let job = WriteFiberJob {
        operation_id: OperationId::from_bytes(pure_id(&format!("{}-op", req.tag))),
        callback_id: CallbackId::from_bytes(pure_id(&format!("{}-cb", req.tag))),
        completion_receipt_id: ReceiptId::from_bytes(pure_id(&format!("{}-op-receipt", req.tag))),
        expected_head_revision: req.expected_head_revision,
        artifact_id: req.artifact_id,
        stage_key,
        stage_bytes: req.bytes.into(),
        stage_created_at_ms: runtime
            .wall_now_ms(pure_key(&format!("{}-stage-clock", req.tag)))
            .expect("stage clock"),
        permit: Some(permit.permit_id),
        write_set_root,
        plan_key: pure_key(&format!("{}-plan", req.tag)),
        planned_at_ms: runtime
            .wall_now_i64(pure_key(&format!("{}-plan-clock", req.tag)))
            .expect("plan clock"),
        task_id: req.task_id,
        attempt_id: req.attempt_id,
        attempt_generation: Generation::INITIAL,
    };
    let spec = FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(pure_id(&format!("{}-fiber", req.tag))),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: req.process.agent_instance_id,
        agent_generation: req.process.agent_instance_generation,
        process_id: req.process.process_id,
        process_generation: req.process.process_generation,
        task_attempt_id: Some(req.attempt_id),
        cancellation_scope_id: req.scope_id,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(pure_id(&format!("{}-rg", req.tag))),
        scheduler_domain_id: SchedulerDomainId::from_bytes(pure_id(&format!("{}-sd", req.tag))),
        deadline: None,
    };
    let (fiber, receiver) = spawn_write_fiber(Arc::clone(runtime), tokio_adapter, spec, job)
        .expect("spawn write fiber");
    let outcome = receiver
        .await
        .expect("write fiber channel")
        .expect("write fiber job");
    assert!(
        outcome.plan_id.is_some(),
        "permit-bound pure-code fiber must record commit plan"
    );
    assert_eq!(tokio_adapter.inspect(fiber), Ok(FiberState::Completed));

    let receipts = runtime
        .converge_pending(
            16,
            runtime
                .wall_now_i64(pure_key(&format!("{}-converge-clock", req.tag)))
                .expect("converge clock"),
        )
        .expect("converge pending commits");
    let receipt = receipts
        .into_iter()
        .find(|r| {
            r.task_receipt.task_id == req.task_id && r.task_receipt.attempt_id == req.attempt_id
        })
        .expect("commit receipt for write attempt");
    (receipt, permit.permit_id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn road_a003_pure_code_zero_model_install_fast_write_crash_recover_uninstall() {
    let dir = TempDir::new("e2e-full");
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let tokio_adapter = adapter();

    // ---- 1. Pure-code package develop / sign / verify / install (zero AgentRole) ----
    let publisher = runtime.bootstrap_publisher(0x10).expect("publisher");
    let package_id = PackageId::from_bytes(pure_id("pure-code-package-v1"));
    let (signed, _entry_artifact) =
        build_pure_code_package(&runtime, &publisher, package_id, "v1.0.0");
    assert!(
        signed
            .tasks
            .iter()
            .all(|node| node.kind == PackageTaskKind::Executable),
        "pure-code package must declare PackageTaskKind::Executable only (zero AgentRole)"
    );

    let verification_decision = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &signed,
                idempotency_key: pure_key("pkg-verify-v1"),
                verified_at_ms: runtime
                    .wall_now_ms(pure_key("pkg-verify-clock-v1"))
                    .expect("verify clock"),
            },
        )
        .expect("verify pure-code package with tasks");
    let verification_receipt = verification_decision.receipt().clone();

    let compiled_proposal = compile_task_templates(
        &signed,
        pure_key("pkg-compile-check"),
        runtime
            .wall_now_ms(pure_key("pkg-compile-clock"))
            .expect("compile clock"),
    )
    .expect("compile pure-code templates");
    assert_eq!(compiled_proposal.nodes.len(), 2);
    assert!(
        compiled_proposal
            .nodes
            .iter()
            .all(|node| node.kind == PlanNodeKind::Executable),
        "compiled plan nodes must all be Executable with zero AgentRole"
    );

    let installation = runtime
        .install_verified_package(&verification_receipt, 0x10)
        .expect("install verified pure-code package");
    let application_id: ApplicationId = installation.application_id;
    assert_eq!(installation.installation_generation.get(), 1);
    let app_view = runtime
        .applications
        .inspect_application(package_id)
        .expect("inspect application")
        .expect("installed application");
    assert_eq!(app_view.status, ApplicationStatus::Installed);
    assert_eq!(app_view.application_id, application_id);

    // ---- 2. Fast zero-model execution -> user Artifact Revision 1 ----
    let user_artifact_id = ArtifactId::from_bytes(pure_id("user-data-ledger-artifact"));
    let artifact_created_at = runtime
        .wall_now_ms(pure_key("user-artifact-create-clock"))
        .expect("user artifact create clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id: user_artifact_id,
            idempotency_key: pure_key("user-artifact-create"),
            content_type: "text/plain".to_string(),
            application_id: Some(application_id),
            owner: Some("user:pure-code-offline".to_string()),
            created_at_ms: artifact_created_at,
        })
        .expect("create user data artifact");

    let (task_id, attempt_rev1, scope_rev1) = runtime
        .register_task_and_attempt_for(0x10, Some(application_id), None)
        .expect("register pure-code task and attempt 1");
    let process_1 = runtime
        .materialize_process(0x10, task_id, attempt_rev1, Generation::INITIAL)
        .expect("materialize process 1");

    let worker_dir = dir.root().join("worker-io");
    let rev1_output_path = worker_dir.join("rev1.out");
    let rev1_report_path = worker_dir.join("rev1.report");
    let rev1_ready_path = worker_dir.join("rev1.ready");
    let rev1_stop_path = worker_dir.join("rev1.stop");

    let mut worker_1 = spawn_isolated_pure_code_worker(
        "compute-and-hold",
        Some("records=alpha:10,beta:25,gamma:65"),
        Some(&rev1_output_path),
        &rev1_report_path,
        &rev1_ready_path,
        Some(&rev1_stop_path),
    );
    worker_1.await_ready_file(&rev1_ready_path, CHILD_WAIT_TIMEOUT);

    let rev1_bytes = fs::read(&rev1_output_path).expect("read rev1 worker output");
    let rev1_sha = sha256_hex(&rev1_bytes);
    assert_env_report_clean(&rev1_report_path, "compute-and-hold", Some(&rev1_sha));
    assert!(
        String::from_utf8_lossy(&rev1_bytes).contains("checksum_sum=100"),
        "pure-code deterministic computation sum must equal 100"
    );

    let registry = SupervisorPidRegistry::new();
    registry
        .register(RegisterSupervisorPidRequest {
            process_id: process_1.process_id,
            process_generation: process_1.process_generation,
            os_pid: worker_1.id(),
            registered_at_ms: runtime
                .wall_now_ms(pure_key("supervisor-reg-clock-1"))
                .expect("supervisor clock"),
        })
        .expect("register real worker_1 OS pid in SupervisorPidRegistry");
    runtime
        .register_background_task(package_id, task_id, publisher.principal_id, 0x10)
        .expect("register background task");
    runtime
        .register_process_binding(
            package_id,
            process_1.process_id,
            publisher.principal_id,
            0x10,
        )
        .expect("register process binding");

    let (commit_rev1, permit_rev1) = execute_commit_write_fiber(
        &runtime,
        &tokio_adapter,
        CommitWriteRequest {
            task_id,
            attempt_id: attempt_rev1,
            scope_id: scope_rev1,
            process: &process_1,
            artifact_id: user_artifact_id,
            expected_head_revision: 0,
            bytes: rev1_bytes.clone(),
            tag: "rev1",
        },
    )
    .await;

    assert_eq!(commit_rev1.task_receipt.task_id, task_id);
    assert_eq!(commit_rev1.task_receipt.permit_id, Some(permit_rev1));
    assert_eq!(commit_rev1.task_receipt.new_head_commit_seq, 1);
    assert_eq!(commit_rev1.artifact_publications.len(), 1);
    assert_eq!(commit_rev1.artifact_publications[0].revision, 1);
    assert_eq!(
        commit_rev1.artifact_publications[0].digest,
        ContentDigest::of_bytes(&rev1_bytes).into_bytes()
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(task_id, attempt_rev1)
            .expect("inspect attempt 1")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_permit(task_id, permit_rev1)
            .expect("inspect permit 1")
            .state,
        PermitState::Closed
    );

    let head_rev1 = runtime
        .artifacts
        .resolve_head(user_artifact_id, u64::MAX)
        .expect("resolve rev1 head")
        .expect("user artifact head exists");
    assert_eq!(head_rev1.revision, 1);
    assert_eq!(head_rev1.digest, ContentDigest::of_bytes(&rev1_bytes));
    let stored_rev1 = runtime
        .artifacts
        .get_revision(user_artifact_id, 1, u64::MAX)
        .expect("readback rev1");
    assert_eq!(stored_rev1, rev1_bytes);
    assert_eq!(sha256_hex(&stored_rev1), rev1_sha);

    // ---- 3. Real SIGKILL crash injection & lossless recovery -> Revision 2 ----
    let (crash_task_id, crash_attempt_id, _crash_scope_id) = runtime
        .register_task_and_attempt_for(0x98, Some(application_id), None)
        .expect("register crash-probe task and attempt");
    let process_2 = runtime
        .materialize_process(0x98, crash_task_id, crash_attempt_id, Generation::INITIAL)
        .expect("materialize process 2");

    let crash_report_path = worker_dir.join("crash.report");
    let crash_ready_path = worker_dir.join("crash.ready");
    let crash_stop_path = worker_dir.join("crash.stop");
    let mut worker_2 = spawn_isolated_pure_code_worker(
        "barrier-before-write",
        None,
        None,
        &crash_report_path,
        &crash_ready_path,
        Some(&crash_stop_path),
    );
    worker_2.await_ready_file(&crash_ready_path, CHILD_WAIT_TIMEOUT);
    assert_env_report_clean(&crash_report_path, "barrier-before-write", None);

    // Inject real OS SIGKILL / TerminateProcess before write/commit.
    let crash_status = worker_2.kill_and_reap();
    assert!(
        !crash_status.success(),
        "killed child must exit unsuccessfully: {crash_status}"
    );
    // Mark process_2 crashed and cancel the aborted pre-permit task/attempt.
    runtime
        .process
        .propagate_crash(PropagateCrashRequest {
            process_id: process_2.process_id,
            expected_process_generation: process_2.process_generation,
            expected_process_fencing_token: process_2.process_fencing_token,
            idempotency_key: pure_key("crash-process2"),
            marked_at_ms: runtime
                .wall_now_ms(pure_key("crash-process2-clock"))
                .expect("crash clock"),
        })
        .expect("mark killed worker process crashed");
    let cancel_crash = runtime
        .tasks
        .cancel_task(CancelRequest {
            task_id: crash_task_id,
            idempotency_key: pure_key("cancel-crashed-task"),
            requested_at_ms: runtime
                .wall_now_i64(pure_key("cancel-crashed-task-clock"))
                .expect("cancel clock"),
        })
        .expect("cancel crashed task");
    assert!(matches!(cancel_crash, CancelDecision::Applied { .. }));
    assert_eq!(
        runtime
            .tasks
            .inspect_attempt(crash_task_id, crash_attempt_id)
            .expect("crashed attempt state")
            .state,
        AttemptState::Cancelled
    );
    // Cancelled attempt cannot obtain a CommitPermit or alter Artifact head.
    let fenced = runtime
        .tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id: crash_task_id,
                attempt_id: crash_attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: [0xEE; 32],
                planned_effects: Vec::new(),
                idempotency_key: pure_key("fenced-crashed-attempt-permit"),
                valid_until_ms: i64::MAX,
                requested_at_ms: runtime
                    .wall_now_i64(pure_key("fenced-crashed-clock"))
                    .expect("fenced clock"),
            },
        )
        .expect("cancelled attempt permit fence check");
    assert!(matches!(
        fenced,
        PermitDecision::CancelledBeforeEffect { .. }
    ));

    let head_after_crash = runtime
        .artifacts
        .resolve_head(user_artifact_id, u64::MAX)
        .expect("resolve head after crash")
        .expect("head remains present");
    assert_eq!(
        head_after_crash.revision, 1,
        "SIGKILL / cancelled attempt must leave authoritative Artifact head at Revision 1"
    );
    assert_eq!(
        head_after_crash.digest,
        ContentDigest::of_bytes(&rev1_bytes)
    );
    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task_1 head after crash")
            .head_commit_seq,
        1
    );
    assert_eq!(
        runtime
            .artifacts
            .list_revisions(user_artifact_id)
            .expect("list revisions after crash")
            .len(),
        1,
        "no revision 2 row may exist after SIGKILL crash"
    );
    assert!(
        runtime
            .tasks
            .list_incomplete_artifact_commit_plans(16)
            .expect("list incomplete plans after crash")
            .is_empty(),
        "no uncommitted plan may exist after pre-write SIGKILL"
    );

    // Launch clean recovery Attempt on `task_1` + third isolated pure-code OS worker.
    let task_head = runtime
        .tasks
        .inspect_task(task_id)
        .expect("inspect task_1 head for recovery");
    let attempt_rev2 = TaskAttemptId::from_bytes(pure_id("attempt-rev2-recover"));
    let scope_rev2 = CancellationScopeId::from_bytes(pure_id("scope-rev2-recover"));
    runtime
        .tasks
        .register_attempt(AttemptSpec {
            task_id,
            attempt_id: attempt_rev2,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes(pure_id("snapshot-rev2-recover")),
                snapshot_digest: Sha256::digest(b"rev2-recover-snapshot").into(),
                expected_head_commit_seq: task_head.head_commit_seq,
                effect_history_root: task_head.head_effect_history_root,
                retry_fence_epoch: task_head.retry_fence_epoch,
            },
            cancellation_scope_id: scope_rev2,
            cancellation_generation: Generation::INITIAL,
            idempotency_key: pure_key("register-attempt-rev2-recover"),
            registered_at_ms: runtime
                .wall_now_i64(pure_key("register-attempt-rev2-clock"))
                .expect("attempt rev2 clock"),
        })
        .expect("register recovery attempt on task_1");
    let process_3 = runtime
        .materialize_process(0xA0, task_id, attempt_rev2, Generation::INITIAL)
        .expect("materialize recovery process 3");

    let rev2_output_path = worker_dir.join("rev2.out");
    let rev2_report_path = worker_dir.join("rev2.report");
    let rev2_ready_path = worker_dir.join("rev2.ready");
    let rev2_input = format!("prev_sha256={rev1_sha};records=alpha:15,beta:35,gamma:50,delta:100");
    let mut worker_3 = spawn_isolated_pure_code_worker(
        "recover-compute",
        Some(&rev2_input),
        Some(&rev2_output_path),
        &rev2_report_path,
        &rev2_ready_path,
        None,
    );
    let recover_status = worker_3.wait_with_timeout(CHILD_WAIT_TIMEOUT);
    assert!(
        recover_status.success(),
        "clean recovery worker_3 must succeed: {recover_status}"
    );
    assert!(rev2_ready_path.is_file());

    let rev2_bytes = fs::read(&rev2_output_path).expect("read rev2 worker output");
    let rev2_sha = sha256_hex(&rev2_bytes);
    assert_env_report_clean(&rev2_report_path, "recover-compute", Some(&rev2_sha));
    assert!(
        String::from_utf8_lossy(&rev2_bytes).contains("checksum_sum=200"),
        "recovery output sum must equal 200"
    );
    assert!(
        String::from_utf8_lossy(&rev2_bytes).contains(&format!("prev_sha256={rev1_sha}")),
        "recovery output must chain rev1 digest"
    );

    let (commit_rev2, permit_rev2) = execute_commit_write_fiber(
        &runtime,
        &tokio_adapter,
        CommitWriteRequest {
            task_id,
            attempt_id: attempt_rev2,
            scope_id: scope_rev2,
            process: &process_3,
            artifact_id: user_artifact_id,
            expected_head_revision: 1,
            bytes: rev2_bytes.clone(),
            tag: "rev2",
        },
    )
    .await;

    assert_eq!(commit_rev2.task_receipt.task_id, task_id);
    assert_eq!(commit_rev2.task_receipt.attempt_id, attempt_rev2);
    assert_eq!(commit_rev2.task_receipt.permit_id, Some(permit_rev2));
    assert_eq!(commit_rev2.task_receipt.prior_head_commit_seq, 1);
    assert_eq!(commit_rev2.task_receipt.new_head_commit_seq, 2);
    assert_eq!(commit_rev2.artifact_publications[0].revision, 2);
    assert_eq!(
        commit_rev2.artifact_publications[0].digest,
        ContentDigest::of_bytes(&rev2_bytes).into_bytes()
    );

    let head_rev2 = runtime
        .artifacts
        .resolve_head(user_artifact_id, u64::MAX)
        .expect("resolve rev2 head")
        .expect("rev2 head present");
    assert_eq!(head_rev2.revision, 2);
    assert_eq!(head_rev2.digest, ContentDigest::of_bytes(&rev2_bytes));
    assert_eq!(
        runtime
            .artifacts
            .get_revision(user_artifact_id, 1, u64::MAX)
            .expect("readback rev1 after rev2 commit"),
        rev1_bytes
    );
    assert_eq!(
        runtime
            .artifacts
            .get_revision(user_artifact_id, 2, u64::MAX)
            .expect("readback rev2 after rev2 commit"),
        rev2_bytes
    );

    // ---- 4. Activity gate + zero-residue teardown + PKG-UNINSTALL-001 UserData preservation ----
    assert_eq!(
        runtime
            .tasks
            .inspect_outstanding_task_count(&[task_id])
            .expect("outstanding task count"),
        1
    );
    let gated_refusal = runtime
        .uninstall_application_gated_by_task_activity(package_id, 0x35)
        .expect_err("active task must block gated uninstall");
    assert!(
        matches!(
            &gated_refusal,
            SliceKError::Application(ApplicationAuthorityError::ApplicationActiveTasksRunning {
                active_task_count: 1,
                ..
            })
        ),
        "unexpected refusal: {gated_refusal:?}"
    );
    assert!(
        worker_1.try_wait().is_none(),
        "worker_1 OS process must remain live until teardown"
    );

    let teardown = run_application_teardown(&runtime, &tokio_adapter, package_id, 0x40, &registry)
        .expect("run application teardown");
    assert_eq!(teardown.application_id, application_id);
    assert_eq!(teardown.kills.len(), 1);
    assert!(matches!(
        teardown.kills[0],
        PlatformKillDecision::Signaled(_)
    ));
    assert_eq!(
        teardown.crashes[0].lifecycle_state,
        ProcessLifecycleState::Crashed
    );
    assert_eq!(teardown.linkages[0].matched_fibers, 1);
    assert_eq!(teardown.linkages[0].already_terminal, 1);
    assert_eq!(teardown.linkages[0].canceled_scopes, 1);
    assert!(matches!(
        teardown.task_cancels[0],
        CancelDecision::Applied {
            cancel_epoch: 1,
            ..
        }
    ));

    #[cfg(unix)]
    {
        let killed_status = worker_1.wait_with_timeout(CHILD_WAIT_TIMEOUT);
        assert!(
            !killed_status.success(),
            "worker_1 must be terminated by teardown SIGTERM: {killed_status}"
        );
    }
    #[cfg(not(unix))]
    {
        write_file_atomic(&rev1_stop_path, b"STOP\n");
        let _ = worker_1.wait_with_timeout(CHILD_WAIT_TIMEOUT);
    }

    assert_eq!(
        runtime
            .tasks
            .inspect_task(task_id)
            .expect("task_1 after teardown")
            .state,
        TaskState::Cancelled
    );
    assert_eq!(
        runtime
            .applications
            .inspect_application(package_id)
            .expect("inspect uninstalled app")
            .expect("app row exists")
            .status,
        ApplicationStatus::Uninstalled
    );
    assert!(matches!(
        tokio_adapter.spawn_fiber(
            FiberSpec {
                fiber_id: ExecutionFiberId::from_bytes(pure_id("post-teardown-probe-fiber")),
                fiber_generation: Generation::INITIAL,
                agent_instance_id: process_1.agent_instance_id,
                agent_generation: process_1.agent_instance_generation,
                process_id: process_1.process_id,
                process_generation: process_1.process_generation,
                task_attempt_id: Some(attempt_rev1),
                cancellation_scope_id: scope_rev1,
                cancellation_generation: Generation::INITIAL,
                resource_group_id: ResourceGroupId::from_bytes(pure_id("post-teardown-rg")),
                scheduler_domain_id: SchedulerDomainId::from_bytes(pure_id("post-teardown-sd")),
                deadline: None,
            },
            Box::pin(std::future::pending()),
        ),
        Err(RuntimeError::Cancelled)
    ));

    // Idempotent teardown replay produces byte-identical receipts.
    let replay = run_application_teardown(&runtime, &tokio_adapter, package_id, 0x40, &registry)
        .expect("teardown replay");
    assert!(matches!(replay.kills[0], PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.crashes, teardown.crashes);
    assert_eq!(
        replay.task_cancels,
        vec![CancelDecision::Replayed { cancel_epoch: 1 }]
    );
    assert_eq!(replay.uninstall, teardown.uninstall);

    // Reinstall of uninstalled package fails closed; orphan GC leaves user Artifact intact.
    let reinstall_err = runtime
        .install_verified_package(&verification_receipt, 0x60)
        .expect_err("reinstall after uninstall must fail closed");
    assert!(matches!(
        reinstall_err,
        SliceKError::Application(ApplicationAuthorityError::ApplicationUninstalled { .. })
    ));
    let (orphan_digest, orphan_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xEE, 64).expect("plant orphan blob");
    let gc = runtime
        .collect_orphan_blobs(0x50)
        .expect("collect orphan blobs after uninstall");
    assert!(matches!(gc, CollectOrphanBlobsDecision::Collected(_)));
    assert_eq!(gc.receipt().collected_digests, vec![orphan_digest]);
    assert!(!orphan_path.exists());

    // Drop and reopen the runtime to verify durable UserData survival across restarts.
    drop(tokio_adapter);
    drop(runtime);
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen runtime post-uninstall");
    let reopened_app = reopened
        .applications
        .inspect_application(package_id)
        .expect("inspect app post-reopen")
        .expect("app row");
    assert_eq!(reopened_app.status, ApplicationStatus::Uninstalled);

    let post_head = reopened
        .artifacts
        .resolve_head(user_artifact_id, u64::MAX)
        .expect("resolve user artifact head post-uninstall")
        .expect("user artifact head preserved");
    assert_eq!(post_head.revision, 2);
    assert_eq!(post_head.digest, ContentDigest::of_bytes(&rev2_bytes));
    let revisions = reopened
        .artifacts
        .list_revisions(user_artifact_id)
        .expect("list user artifact revisions post-uninstall");
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].revision, 1);
    assert_eq!(revisions[1].revision, 2);

    let post_rev1 = reopened
        .artifacts
        .get_revision(user_artifact_id, 1, u64::MAX)
        .expect("user artifact rev 1 readable after uninstall");
    let post_rev2 = reopened
        .artifacts
        .get_revision(user_artifact_id, 2, u64::MAX)
        .expect("user artifact rev 2 readable after uninstall");
    assert_eq!(post_rev1, rev1_bytes);
    assert_eq!(post_rev2, rev2_bytes);
    assert_eq!(sha256_hex(&post_rev1), rev1_sha);
    assert_eq!(sha256_hex(&post_rev2), rev2_sha);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::too_many_lines)]
async fn road_a003_pure_code_negative_tampered_package_and_env_guard() {
    let dir = TempDir::new("e2e-negative");
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let tokio_adapter = adapter();

    // ---- 1. Tampered pure-code manifest fails verification with zero durable writes ----
    let publisher = runtime.bootstrap_publisher(0x20).expect("publisher");
    let package_id = PackageId::from_bytes(pure_id("pure-code-neg-package"));
    let (signed, _entry_artifact) =
        build_pure_code_package(&runtime, &publisher, package_id, "v1.0.0-neg");

    let mut tampered_version = signed.clone();
    tampered_version.manifest.version = pack_package_version(9, 9, 9);
    let err_version = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &tampered_version,
                idempotency_key: pure_key("neg-tamper-version"),
                verified_at_ms: runtime
                    .wall_now_ms(pure_key("neg-tamper-version-clock"))
                    .expect("tamper version clock"),
            },
        )
        .expect_err("tampered manifest version must fail verification");
    assert!(
        matches!(err_version, ArtifactError::PackageSignatureInvalid),
        "expected PackageSignatureInvalid, got: {err_version}"
    );

    let mut tampered_task_kind = signed.clone();
    tampered_task_kind.tasks[0].kind = PackageTaskKind::AgentRole;
    let err_task_kind = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &tampered_task_kind,
                idempotency_key: pure_key("neg-tamper-task-kind"),
                verified_at_ms: runtime
                    .wall_now_ms(pure_key("neg-tamper-task-kind-clock"))
                    .expect("tamper task kind clock"),
            },
        )
        .expect_err("tampered task kind must fail verification");
    assert!(
        matches!(err_task_kind, ArtifactError::PackageSignatureInvalid),
        "expected PackageSignatureInvalid, got: {err_task_kind}"
    );

    assert!(
        runtime
            .applications
            .inspect_application(package_id)
            .expect("inspect after tampered verify")
            .is_none(),
        "tampered verification must write zero application rows"
    );
    assert!(
        runtime
            .plans()
            .list_plan_ids()
            .expect("list plans after tampered verify")
            .is_empty(),
        "tampered verification must write zero plan rows"
    );

    // ---- 2. Active task / unbound supervisor refusal before teardown ----
    let valid_decision = runtime
        .artifacts
        .verify_package_with_tasks(
            &runtime.identity,
            VerifyPackageWithTasksRequest {
                signed: &signed,
                idempotency_key: pure_key("neg-valid-verify"),
                verified_at_ms: runtime
                    .wall_now_ms(pure_key("neg-valid-verify-clock"))
                    .expect("valid verify clock"),
            },
        )
        .expect("verify honest pure-code package");
    let installation = runtime
        .install_verified_package(valid_decision.receipt(), 0x20)
        .expect("install pure-code package");

    let (task_id, attempt_id, _scope_id) = runtime
        .register_task_and_attempt_for(0x20, Some(installation.application_id), None)
        .expect("register negative-case task");
    let process = runtime
        .materialize_process(0x20, task_id, attempt_id, Generation::INITIAL)
        .expect("materialize negative-case process");
    runtime
        .register_background_task(package_id, task_id, publisher.principal_id, 0x20)
        .expect("register background task");
    runtime
        .register_process_binding(package_id, process.process_id, publisher.principal_id, 0x20)
        .expect("register process binding");

    let worker_dir = dir.root().join("neg-worker");
    let hold_output = worker_dir.join("hold.out");
    let hold_report = worker_dir.join("hold.report");
    let hold_ready = worker_dir.join("hold.ready");
    let hold_stop = worker_dir.join("hold.stop");
    let mut live_worker = spawn_isolated_pure_code_worker(
        "compute-and-hold",
        Some("records=item:42"),
        Some(&hold_output),
        &hold_report,
        &hold_ready,
        Some(&hold_stop),
    );
    live_worker.await_ready_file(&hold_ready, CHILD_WAIT_TIMEOUT);

    let refused_active = runtime
        .uninstall_application_gated_by_task_activity(package_id, 0x25)
        .expect_err("uninstall must refuse while task is active");
    assert!(matches!(
        refused_active,
        SliceKError::Application(ApplicationAuthorityError::ApplicationActiveTasksRunning {
            active_task_count: 1,
            ..
        })
    ));
    assert_eq!(
        runtime
            .applications
            .inspect_application(package_id)
            .expect("inspect after active refusal")
            .expect("app row")
            .status,
        ApplicationStatus::Installed
    );

    // On Unix, teardown with an unbound (empty) SupervisorPidRegistry fails closed
    // before uninstalling; request_platform_kill commits its durable receipt
    // at-least-once before consulting the adapter, and then the retry with the
    // populated registry completes signaling and uninstall.
    let registry = SupervisorPidRegistry::new();
    #[cfg(unix)]
    {
        let empty_registry = SupervisorPidRegistry::new();
        let Err(unbound_err) =
            run_application_teardown(&runtime, &tokio_adapter, package_id, 0x28, &empty_registry)
        else {
            panic!("teardown without supervisor PID binding must fail closed on Unix");
        };
        assert!(matches!(
            unbound_err,
            SliceKError::Process(ProcessAuthorityError::PlatformKillAdapter(_))
        ));
        assert_eq!(
            runtime
                .applications
                .inspect_application(package_id)
                .expect("inspect after unbound teardown refusal")
                .expect("app row")
                .status,
            ApplicationStatus::Installed
        );
    }

    registry
        .register(RegisterSupervisorPidRequest {
            process_id: process.process_id,
            process_generation: process.process_generation,
            os_pid: live_worker.id(),
            registered_at_ms: runtime
                .wall_now_ms(pure_key("neg-supervisor-clock"))
                .expect("supervisor clock"),
        })
        .expect("register negative worker pid");
    let teardown = run_application_teardown(&runtime, &tokio_adapter, package_id, 0x28, &registry)
        .expect("teardown succeeds once PID bound");
    assert_eq!(teardown.application_id, installation.application_id);
    #[cfg(unix)]
    {
        let status = live_worker.wait_with_timeout(CHILD_WAIT_TIMEOUT);
        assert!(!status.success());
    }
    #[cfg(not(unix))]
    {
        write_file_atomic(&hold_stop, b"STOP\n");
        let _ = live_worker.wait_with_timeout(CHILD_WAIT_TIMEOUT);
    }
    assert_eq!(
        runtime
            .applications
            .inspect_application(package_id)
            .expect("inspect after cleanup")
            .expect("app row")
            .status,
        ApplicationStatus::Uninstalled
    );

    // ---- 3. Two-hop `.env_clear()` isolation proof from poisoned parent env ----
    let relay_report = worker_dir.join("relay.report");
    let relay_ready = worker_dir.join("relay.ready");
    let grandchild_report = worker_dir.join("grandchild.report");
    let grandchild_ready = worker_dir.join("grandchild.ready");

    let exe = std::env::current_exe().expect("current_exe");
    let mut poisoned_parent = ChildGuard::new(
        Command::new(exe)
            .args(["--exact", WORKER_TEST_NAME, "--ignored", "--nocapture"])
            .env(ENV_WORKER, "1")
            .env(ENV_MODE, "relay-with-env-clear")
            .env(ENV_REPORT, &relay_report)
            .env(ENV_READY, &relay_ready)
            .env(ENV_GRANDCHILD_REPORT, &grandchild_report)
            .env(ENV_GRANDCHILD_READY, &grandchild_ready)
            .env("OPENAI_API_KEY", "sk-poisoned-openai-sentinel")
            .env("ANTHROPIC_API_KEY", "sk-ant-poisoned-sentinel")
            .env("GLM_API_KEY", "glm-poisoned-sentinel")
            .env("ZHIPU_API_KEY", "zhipu-poisoned-sentinel")
            .env("LLMOS_KEYPOOL_CONFIG", "/tmp/poisoned-keypool.json")
            .env("LLMOS_LLM_BASE_URL", "http://127.0.0.1:9/v1")
            .env("HTTP_PROXY", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env("ALL_PROXY", "socks5://127.0.0.1:9")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn poisoned parent probe"),
    );
    let relay_status = poisoned_parent.wait_with_timeout(CHILD_WAIT_TIMEOUT);
    assert!(
        relay_status.success(),
        "poisoned-parent relay probe failed: {relay_status}"
    );
    assert!(relay_ready.is_file());
    assert!(grandchild_ready.is_file());

    let relay_text = fs::read_to_string(&relay_report).expect("read relay report");
    assert!(
        relay_text.contains("poisoned_keys_seen=9"),
        "control direction must prove poisoned parent observed all 9 injected keys:\n{relay_text}"
    );
    assert_env_report_clean(&grandchild_report, "env-audit-only", None);
}
