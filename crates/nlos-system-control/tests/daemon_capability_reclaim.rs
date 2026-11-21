//! W82-INT capability-revocation -> managed worker process-tree reclaim
//! integration tests (`daemon` feature).

#![cfg(feature = "daemon")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer as _, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityHandle, CapabilityRecord, CapabilityRights, CapabilityTarget,
    DelegateCapabilityRequest, RevokeCapabilityRequest, SignedDelegateCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, revoke_command_message,
};
use nlos_identity::{IdentityAuthority, IdentityBinding};
use nlos_process::{
    CreateIsolationDomainRequest, KillScope, PlatformKillDecision, ProcessAuthority,
    ProcessBindingRecord, RegisterDelegatedProcessRequest, RegisterSupervisorPidRequest,
};
use nlos_system_control::daemon::{
    CapabilityInvalidationReason, CapabilityRevocationReclaimReport,
    CapabilityWorkerBindingDecision, CapabilityWorkerReclaimError, CapabilityWorkerReclaimReceipt,
    DaemonOptions, SystemControlDaemon, assemble,
};
use nlos_types::{Generation, IdempotencyKey, NamespaceId, TaskAttemptId, TaskId};

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-w82int-reclaim-{label}-{}-{seq}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn as_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("child still owned")
    }

    fn take(&mut self) -> Child {
        self.0.take().expect("child still owned")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct TempKeyFile(PathBuf);

impl TempKeyFile {
    fn from_seed(label: &str, seed: &[u8; 32]) -> Self {
        let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-w82int-key-{label}-{}-{seq}.key",
            std::process::id()
        ));
        let mut hex = String::with_capacity(64);
        for byte in seed {
            use std::fmt::Write as _;
            let _ = write!(hex, "{byte:02x}");
        }
        fs::write(&path, hex).expect("write operator seed file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod 0600");
        }
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempKeyFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn temp_socket(label: &str) -> PathBuf {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-w82int-{label}-{}-{seq}.sock",
        std::process::id()
    ))
}

#[cfg(windows)]
fn temp_socket(label: &str) -> PathBuf {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-w82int-{label}-{}-{seq}",
        std::process::id()
    ))
}

fn process_fixture(authority: &ProcessAuthority, seed: u8) -> ProcessBindingRecord {
    let domain = authority
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed; 32],
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            created_at_ms: 1_000,
        })
        .expect("create isolation domain")
        .record()
        .clone();
    authority
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed; 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: domain.isolation_domain_id,
            isolation_domain_generation: domain.generation,
            isolation_domain_fencing_token: domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
            created_at_ms: 1_100,
        })
        .expect("register delegated process")
        .record()
        .clone()
}

fn delegate_child_cap(
    authority: &CapabilityAuthority,
    identity: &IdentityAuthority,
    parent: &CapabilityRecord,
    operator: IdentityBinding,
    signing_key: &SigningKey,
    key_byte: u8,
) -> CapabilityRecord {
    let command = DelegateCapabilityRequest {
        parent: parent.handle,
        delegator_key_id: operator.key_id,
        recipient_key_id: operator.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0; 16])),
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: None,
        valid_from_ms: 0,
        valid_until_ms: parent.valid_until_ms,
        delegation_depth_remaining: 0,
        call_limit: None,
        idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
        delegated_at_ms: 1_000,
    };
    authority
        .delegate_signed(
            identity,
            SignedDelegateCapabilityRequest {
                command,
                signer: operator.principal_id,
                signature: signing_key
                    .sign(&delegate_command_message(command))
                    .to_bytes(),
            },
        )
        .expect("delegate child capability")
        .record()
}

fn revoke_cap(
    authority: &CapabilityAuthority,
    identity: &IdentityAuthority,
    handle: CapabilityHandle,
    operator: IdentityBinding,
    signing_key: &SigningKey,
    key_byte: u8,
    revoked_at_ms: u64,
) {
    let command = RevokeCapabilityRequest {
        handle,
        revoker_key_id: operator.key_id,
        idempotency_key: IdempotencyKey::from_bytes([key_byte; 16]),
        revoked_at_ms,
    };
    authority
        .revoke_signed(
            identity,
            SignedRevokeCapabilityRequest {
                command,
                signer: operator.principal_id,
                signature: signing_key
                    .sign(&revoke_command_message(command))
                    .to_bytes(),
            },
        )
        .expect("revoke capability");
}

fn assert_no_reclaims_or_errors(report: &CapabilityRevocationReclaimReport) {
    assert_eq!(report.reclaimed, [] as [CapabilityWorkerReclaimReceipt; 0]);
    assert_eq!(report.errors, [] as [CapabilityWorkerReclaimError; 0]);
}

#[cfg(unix)]
fn os_pid_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn spawn_grouped_tree_with_grandchild(pid_file: &Path) -> (ChildGuard, u32) {
    use std::os::unix::process::CommandExt as _;

    let pid_file_str = pid_file
        .to_str()
        .expect("temp path must be valid UTF-8")
        .to_owned();
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c")
        .arg("sleep 60 & echo $! > \"$1\"; wait")
        .arg("sh")
        .arg(&pid_file_str)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let child = ChildGuard::new(cmd.spawn().expect("spawn grouped shell tree"));

    let deadline = Instant::now() + Duration::from_secs(5);
    let grandchild_pid = loop {
        if let Ok(raw) = fs::read_to_string(pid_file)
            && let Ok(pid) = raw.trim().parse::<u32>()
        {
            break pid;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for grandchild pid file"
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(os_pid_alive(grandchild_pid), "grandchild must start alive");
    (child, grandchild_pid)
}

#[cfg(unix)]
fn bind_grouped_worker(
    daemon: &SystemControlDaemon,
    cap: CapabilityHandle,
    proc_binding: &ProcessBindingRecord,
    os_pid: u32,
) {
    daemon
        .register_supervisor_pid_at_scope(
            RegisterSupervisorPidRequest {
                process_id: proc_binding.process_id,
                process_generation: proc_binding.process_generation,
                os_pid,
                registered_at_ms: 1_200,
            },
            KillScope::ProcessGroup,
        )
        .expect("register grouped supervisor pid");
    assert!(matches!(
        daemon
            .bind_capability_worker(
                cap.capability_id,
                proc_binding.process_id,
                proc_binding.process_generation,
            )
            .expect("bind capability worker"),
        CapabilityWorkerBindingDecision::Registered(_)
    ));
}

#[cfg(unix)]
fn assert_signaled_and_drained(mut child: ChildGuard, grandchild_pid: Option<u32>) {
    use std::os::unix::process::ExitStatusExt as _;

    let mut owned = child.take();
    let status = owned.wait().expect("wait terminated worker");
    assert!(!status.success());
    assert!(status.signal().is_some(), "worker must exit by signal");
    if let Some(pid) = grandchild_pid {
        let drain_deadline = Instant::now() + Duration::from_secs(2);
        while os_pid_alive(pid) && Instant::now() < drain_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !os_pid_alive(pid),
            "grandchild {pid} in worker process group must be terminated"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unconfigured_capabilities_reclaim_has_zero_side_effects() {
    let root = TempRoot::new("nocap");
    let auth_socket = temp_socket("nocap-a");
    let plain_socket = temp_socket("nocap-p");

    let mut child = {
        #[cfg(unix)]
        {
            ChildGuard::new(
                Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn sleep child"),
            )
        }
        #[cfg(windows)]
        {
            ChildGuard::new(
                Command::new("cmd")
                    .args(["/C", "ping -n 60 127.0.0.1 >NUL"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("spawn ping child"),
            )
        }
    };

    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble unconfigured daemon");

    let proc_binding = process_fixture(&daemon.process, 0x21);
    daemon
        .register_supervisor_pid(RegisterSupervisorPidRequest {
            process_id: proc_binding.process_id,
            process_generation: proc_binding.process_generation,
            os_pid: child.as_mut().id(),
            registered_at_ms: 1_200,
        })
        .expect("register supervisor pid");

    let cap_id = nlos_types::CapabilityId::from_bytes([0x99; 16]);
    assert!(matches!(
        daemon
            .bind_capability_worker(
                cap_id,
                proc_binding.process_id,
                proc_binding.process_generation,
            )
            .expect("bind capability worker"),
        CapabilityWorkerBindingDecision::Registered(_)
    ));

    let report = daemon.reclaim_revoked_capability_workers(2_000);
    assert!(!report.authority_configured);
    assert_eq!(report.checked_bindings, 1);
    assert_eq!(report.active_bindings, 0);
    assert_eq!(report.revoked_detected, 0);
    assert_no_reclaims_or_errors(&report);
    assert!(
        child.as_mut().try_wait().expect("try_wait").is_none(),
        "unconfigured daemon must not terminate any bound worker"
    );

    daemon.stop_worker();
    daemon.stop_materialization();
    drop(endpoints);
}

#[cfg(unix)]
struct RevocationHarness {
    _root: TempRoot,
    _key_file: TempKeyFile,
    signing_key: SigningKey,
    daemon: std::sync::Arc<SystemControlDaemon>,
    endpoints: nlos_system_control::daemon::DaemonEndpoints,
    root_handle: CapabilityHandle,
    cap_a: CapabilityHandle,
    operator: IdentityBinding,
    proc_a: ProcessBindingRecord,
    proc_b: ProcessBindingRecord,
    child_a: Option<ChildGuard>,
    grandchild_a_pid: u32,
    child_b: Option<ChildGuard>,
}

#[cfg(unix)]
fn setup_revocation_harness() -> RevocationHarness {
    use std::os::unix::process::CommandExt as _;

    let root = TempRoot::new("revoke-tree");
    let seed = [0x41_u8; 32];
    let key_file = TempKeyFile::from_seed("operator", &seed);
    let signing_key = SigningKey::from_bytes(&seed);
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(temp_socket("revoke-a"))
            .with_plain_socket(temp_socket("revoke-p"))
            .with_operator_key_file(key_file.path()),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon with operator capability authority");

    let cap_authority = daemon.capabilities.as_ref().expect("capability authority");
    let root_record = daemon.operator_root.as_ref().expect("operator root").record;
    let operator = daemon
        .identity
        .inspect_current_principal_binding(root_record.holder)
        .expect("inspect operator identity");

    let cap_a = delegate_child_cap(
        cap_authority,
        &daemon.identity,
        &root_record,
        operator,
        &signing_key,
        0xA1,
    );
    let cap_b = delegate_child_cap(
        cap_authority,
        &daemon.identity,
        &root_record,
        operator,
        &signing_key,
        0xB1,
    );
    let proc_a = process_fixture(&daemon.process, 0x51);
    let proc_b = process_fixture(&daemon.process, 0x52);

    let (mut child_a, grandchild_a_pid) =
        spawn_grouped_tree_with_grandchild(&root.path().join("grandchild_a.pid"));
    let mut child_b = ChildGuard::new(
        Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn grouped sibling B"),
    );

    bind_grouped_worker(&daemon, cap_a.handle, &proc_a, child_a.as_mut().id());
    bind_grouped_worker(&daemon, cap_b.handle, &proc_b, child_b.as_mut().id());

    RevocationHarness {
        _root: root,
        _key_file: key_file,
        signing_key,
        daemon,
        endpoints,
        root_handle: root_record.handle,
        cap_a: cap_a.handle,
        operator,
        proc_a,
        proc_b,
        child_a: Some(child_a),
        grandchild_a_pid,
        child_b: Some(child_b),
    }
}

#[cfg(unix)]
fn assert_pre_revocation_clean(harness: &mut RevocationHarness) {
    let pre = harness.daemon.reclaim_revoked_capability_workers(2_000);
    assert!(pre.authority_configured);
    assert_eq!(
        (
            pre.checked_bindings,
            pre.active_bindings,
            pre.revoked_detected
        ),
        (2, 2, 0)
    );
    assert_no_reclaims_or_errors(&pre);
    assert!(
        harness
            .child_a
            .as_mut()
            .unwrap()
            .as_mut()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert!(os_pid_alive(harness.grandchild_a_pid));
    assert!(
        harness
            .child_b
            .as_mut()
            .unwrap()
            .as_mut()
            .try_wait()
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
fn assert_direct_revocation_reclaims_a(harness: &mut RevocationHarness) {
    let cap_authority = harness
        .daemon
        .capabilities
        .as_ref()
        .expect("capability authority");
    revoke_cap(
        cap_authority,
        &harness.daemon.identity,
        harness.cap_a,
        harness.operator,
        &harness.signing_key,
        0xC1,
        3_000,
    );

    let post = harness.daemon.reclaim_revoked_capability_workers(3_100);
    assert_eq!(
        (
            post.checked_bindings,
            post.active_bindings,
            post.revoked_detected
        ),
        (2, 1, 1)
    );
    assert_eq!(post.errors, [] as [CapabilityWorkerReclaimError; 0]);
    assert_eq!(post.reclaimed.len(), 1);
    assert_eq!(
        post.reclaimed[0].reason,
        CapabilityInvalidationReason::GenerationConflict
    );
    assert_eq!(post.reclaimed[0].process_id, harness.proc_a.process_id);
    assert_eq!(post.reclaimed[0].kill_scope, KillScope::ProcessGroup);
    assert!(matches!(
        post.reclaimed[0].kill_decision,
        PlatformKillDecision::Signaled(_)
    ));

    assert_signaled_and_drained(
        harness.child_a.take().unwrap(),
        Some(harness.grandchild_a_pid),
    );
    assert!(
        harness
            .daemon
            .process
            .inspect_platform_kill_receipt(
                harness.proc_a.process_id,
                harness.proc_a.process_generation
            )
            .expect("receipt A")
            .is_some()
    );
    assert!(
        harness
            .daemon
            .process
            .inspect_platform_kill_receipt(
                harness.proc_b.process_id,
                harness.proc_b.process_generation
            )
            .expect("receipt B")
            .is_none()
    );
    assert!(
        harness
            .child_b
            .as_mut()
            .unwrap()
            .as_mut()
            .try_wait()
            .unwrap()
            .is_none()
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoking_bound_capability_reclaims_process_tree_and_preserves_siblings() {
    let mut harness = setup_revocation_harness();
    assert_pre_revocation_clean(&mut harness);
    assert_direct_revocation_reclaims_a(&mut harness);

    let followup = harness.daemon.reclaim_revoked_capability_workers(3_200);
    assert_eq!(
        (
            followup.checked_bindings,
            followup.active_bindings,
            followup.revoked_detected
        ),
        (1, 1, 0)
    );
    assert_no_reclaims_or_errors(&followup);

    let cap_authority = harness
        .daemon
        .capabilities
        .as_ref()
        .expect("capability authority");
    revoke_cap(
        cap_authority,
        &harness.daemon.identity,
        harness.root_handle,
        harness.operator,
        &harness.signing_key,
        0xD1,
        3_300,
    );
    let cascade = harness.daemon.reclaim_revoked_capability_workers(3_400);
    assert_eq!(cascade.reclaimed.len(), 1);
    assert_eq!(
        cascade.reclaimed[0].reason,
        CapabilityInvalidationReason::AncestorRevokedOrFenced
    );
    assert_signaled_and_drained(harness.child_b.take().unwrap(), None);

    harness.daemon.stop_worker();
    harness.daemon.stop_materialization();
    drop(harness.endpoints);
}
