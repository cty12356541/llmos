//! W82-C 端到端:真实 `nlos-up workers` 二进制 + 双单元清单 + 三变量
//! 白名单 + SIGTERM 主进程 → 子进程树(含孙)干净回收。unix 门控。
#![cfg(unix)]
// 测试体取自任务书(W82-C Task 4 brief);样式 lint 就地豁免。python 断言
// 自 2026-11-11 起按 Task 3 基线探测裁定归一——硬编码名单在 CI macOS 因
// CoreFoundation 按 uid 自合成 __CF_USER_TEXT_ENCODING 必红(实证见 CI)。
#![allow(clippy::collapsible_if, clippy::map_unwrap_or)]

mod workers_readiness;

use ed25519_dalek::{Signer as _, SigningKey};
use nlos_capability::{
    CapabilityAuthority, CapabilityHandle, CapabilityRights, CapabilityTarget,
    DelegateCapabilityRequest, RevokeCapabilityRequest, SignedDelegateCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, revoke_command_message,
};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_types::{Generation, IdempotencyKey, NamespaceId};
use nlos_up::{BootstrapOptions, bootstrap_trial_root, encode_hex};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use workers_readiness::workers_are_ready;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let base = if Path::new("/tmp").is_dir() {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!(
            "w82c-up-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn poll_file(path: &Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path) {
            if !text.trim().is_empty() {
                return Some(text);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

fn parse_env_dump(text: &str) -> std::collections::BTreeMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// 同一解释器在空环境(`env_clear`)下自合成的变量名集合(基线探测,
/// 与 nlos-process `managed_env.rs` 同型裁定):自合成集合因解释器构建
/// 而异(PEP 538 `LC_CTYPE`;macOS 系统构建另有 CoreFoundation 按 uid
/// 合成的 `__CF_USER_TEXT_ENCODING` 等),硬编码名单在 CI macOS 必红;
/// 受管子进程允许「基线 ∪ LLMOS_*」,父环境零泄漏的检验力不变。
fn python_self_synthesized_env(python: &Path) -> Vec<String> {
    let output = Command::new(python)
        .args(["-c", "import os\nfor key in sorted(os.environ): print(key)"])
        .env_clear()
        .output()
        .expect("baseline python probe spawns");
    assert!(
        output.status.success(),
        "baseline python probe failed: {output:?}"
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let ok = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 运行中的 nlos-up workers:后台线程全量收集 stdout(直到 EOF),
/// 主线程轮询关键行;SIGTERM 后 join 收全输出。
struct RunningUp {
    child: Child,
    stdout: std::sync::Arc<std::sync::Mutex<String>>,
    reader: Option<std::thread::JoinHandle<()>>,
}

impl RunningUp {
    fn wait_for_workers(&self, expected_workers: usize, seconds: u32) -> String {
        for _ in 0..(seconds * 20) {
            if let Ok(buf) = self.stdout.lock() {
                if workers_are_ready(&buf, expected_workers) {
                    return buf.clone();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "nlos-up never reported all {expected_workers} workers ready:\n{}",
            self.snapshot()
        );
    }

    fn snapshot(&self) -> String {
        self.stdout
            .lock()
            .map(|buf| buf.clone())
            .unwrap_or_default()
    }

    fn sigterm_and_collect(self) -> (String, std::process::ExitStatus) {
        let pid = self.child.id();
        let _ = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        self.wait_and_collect()
    }

    /// 等待 nlos-up 自行退出并收全输出(不发送信号;部分失败场景下
    /// SIGTERM 可能落在 bootstrap 早期,把被测行为误杀)。
    fn wait_and_collect(mut self) -> (String, std::process::ExitStatus) {
        let status = self.child.wait().expect("wait nlos-up exit");
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        (self.snapshot(), status)
    }
}

fn spawn_nlos_up_workers(root: &Path, manifest: &Path) -> RunningUp {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nlos-up"));
    command.args([
        "workers",
        "--root",
        &root.display().to_string(),
        "--workers-manifest",
        &manifest.display().to_string(),
    ]);
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn nlos-up workers");
    let stdout = child.stdout.take().expect("piped stdout");
    let shared = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let sink = std::sync::Arc::clone(&shared);
    let reader = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        for line in BufReader::new(stdout).lines() {
            match line {
                Ok(line) => {
                    if let Ok(mut buf) = sink.lock() {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
                Err(_) => break,
            }
        }
    });
    RunningUp {
        child,
        stdout: shared,
        reader: Some(reader),
    }
}

fn assert_dual_sh_whitelist_and_collect_pids(
    scratch: &ScratchDir,
    root: &Path,
    slot: &str,
) -> Vec<(String, u32, u32)> {
    let mut pids = Vec::new();
    for unit in ["adapter", "brain"] {
        let base = scratch.0.join(unit);
        let dump = poll_file(&base.with_extension("env"), 30)
            .unwrap_or_else(|| panic!("{unit} env dump missing"));
        let map = parse_env_dump(&dump);
        // 恰三 LLMOS_* 注入;PWD/SHLVL/_ 为 bash-as-sh 子进程自合成(Task 3 裁定,
        // 非父泄漏);父环境变量(PATH/HOME 等,cargo test 父必带)零泄漏
        for required in [
            "LLMOS_AUTH_ENDPOINT",
            "LLMOS_PROCESS_IDENTITY_SEED",
            "LLMOS_ROOT_CAPABILITY_SLOT",
        ] {
            assert!(
                map.contains_key(required),
                "{unit} missing {required}: {map:?}"
            );
        }
        let leaked: Vec<&String> = map
            .keys()
            .filter(|key| {
                !key.starts_with("LLMOS_") && !matches!(key.as_str(), "PWD" | "SHLVL" | "_")
            })
            .collect();
        assert!(leaked.is_empty(), "{unit} child env leaked: {leaked:?}");
        assert!(
            !map.contains_key("PATH") && !map.contains_key("HOME"),
            "{unit}: parent env leaked: {map:?}"
        );
        assert_eq!(
            map.get("LLMOS_AUTH_ENDPOINT").unwrap(),
            &root.join("system-control-auth.sock").display().to_string()
        );
        assert_eq!(
            map.get("LLMOS_PROCESS_IDENTITY_SEED").unwrap(),
            &root.join("keys/operator-ed25519.key").display().to_string()
        );
        assert_eq!(map.get("LLMOS_ROOT_CAPABILITY_SLOT").unwrap(), slot);
        let pid: u32 = poll_file(&base.with_extension("pid"), 5)
            .unwrap_or_else(|| panic!("{unit} pid file missing"))
            .trim()
            .parse()
            .unwrap();
        let grandpid: u32 = poll_file(&base.with_extension("grandpid"), 5)
            .unwrap_or_else(|| panic!("{unit} grandpid file missing"))
            .trim()
            .parse()
            .unwrap();
        pids.push((unit.to_owned(), pid, grandpid));
    }
    pids
}

fn assert_dual_reclaim_output_and_pids_gone(
    stdout: &str,
    status: std::process::ExitStatus,
    pids: &[(String, u32, u32)],
) {
    assert!(status.success(), "exit {:?}\n{stdout}", status.code());
    assert!(stdout.contains("KILL name=adapter outcome="), "{stdout}");
    assert!(stdout.contains("KILL name=brain outcome="), "{stdout}");
    assert!(
        stdout.contains("STOPPED service=system_control"),
        "{stdout}"
    );
    for (unit, pid, grandpid) in pids {
        assert!(
            pid_gone(*pid, 20),
            "worker {unit} pid {pid} survived main-exit reclaim"
        );
        assert!(
            pid_gone(*grandpid, 20),
            "grandchild of {unit} pid {grandpid} survived main-exit reclaim"
        );
    }
}

#[test]
fn workers_launch_env_whitelist_and_sigterm_reclaims_tree() {
    let scratch = ScratchDir::new("e2e");
    let root = scratch.0.join("root");
    let receipt = bootstrap_trial_root(
        &BootstrapOptions::new()
            .with_root(&root)
            .without_default_keypool_probe(),
    )
    .expect("bootstrap trial root for real operator capability");
    let real_cap_slot = receipt.operator_root_capability_hex.clone();
    assert_eq!(real_cap_slot.len(), 32);

    let manifest_path = scratch.0.join("workers.manifest");
    std::fs::write(
        &manifest_path,
        format!(
            "# W82-C e2e dual-unit manifest (bound to real authority CapabilityId)\n\
             worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter {real_cap_slot}\n\
             worker = brain /bin/sh {sh} {b}\n\
             slot = brain {real_cap_slot}\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = scratch.0.join("adapter").display(),
            b = scratch.0.join("brain").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let stdout_head = up.wait_for_workers(2, 60);
    assert!(stdout_head.contains("WORKERS manifest="), "{stdout_head}");
    assert!(stdout_head.contains("units=2"), "{stdout_head}");
    assert!(
        stdout_head.contains(&format!("operator_root_capability={real_cap_slot}")),
        "{stdout_head}"
    );
    assert!(
        stdout_head.contains("WORKER name=adapter pid="),
        "{stdout_head}"
    );
    assert!(
        stdout_head.contains("WORKER name=brain pid="),
        "{stdout_head}"
    );

    let pids = assert_dual_sh_whitelist_and_collect_pids(&scratch, &root, &real_cap_slot);
    let (stdout, status) = up.sigterm_and_collect();
    assert_dual_reclaim_output_and_pids_gone(&stdout, status, &pids);
}

#[test]
fn workers_python_unit_env_whitelist_gate_letter() {
    let Some(python) = resolve_python3() else {
        panic!("python3 required for the W82-C gate-letter e2e (acceptance: 拉起外部 Python 组件)");
    };
    let scratch = ScratchDir::new("py");
    let root = scratch.0.join("root");
    let manifest_path = scratch.0.join("workers.manifest");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = pyworker {py} {script} {base}\n",
            py = python.display(),
            script = fixture("worker_env_dump.py").display(),
            base = scratch.0.join("pyw").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let stdout_head = up.wait_for_workers(1, 60);
    assert!(
        stdout_head.contains("WORKER name=pyworker"),
        "{stdout_head}"
    );

    let dump = poll_file(&scratch.0.join("pyw.env"), 30).expect("python env dump");
    let map = parse_env_dump(&dump);
    // 解释器自合成集合以基线探测归一(同 managed_env.rs 裁定);其余非
    // LLMOS_* 即泄漏(PATH/HOME 等父环境变量不在基线内,仍被拦截)
    let baseline = python_self_synthesized_env(&python);
    let leaked: Vec<&String> = map
        .keys()
        .filter(|key| !key.starts_with("LLMOS_") && !baseline.contains(key))
        .collect();
    assert!(leaked.is_empty(), "python child env leaked: {leaked:?}");
    assert!(map.contains_key("LLMOS_AUTH_ENDPOINT"));

    let (_stdout, status) = up.sigterm_and_collect();
    assert!(status.success());
    let pid: u32 = poll_file(&scratch.0.join("pyw.pid"), 5)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid_gone(pid, 20), "python worker survived reclaim");
}

/// 部分拉起失败回收(W82-C fix 轮):unit 1 正常拉起后 unit 2 解释器
/// 不可解析——nlos-up 必须先回收已拉起的 worker 树(KILL 回执)再以
/// 非零码退出;直子与孙进程均不得泄漏存活。
#[test]
fn workers_partial_spawn_failure_still_reclaims_launched_trees() {
    let scratch = ScratchDir::new("partial");
    let root = scratch.0.join("root");
    let manifest_path = scratch.0.join("workers.manifest");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = good /bin/sh {sh} {base}\n\
             worker = doomed w82c-no-such-interpreter-xyz placeholder\n",
            sh = fixture("worker_env_dump.sh").display(),
            base = scratch.0.join("good").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let (stdout, status) = up.wait_and_collect();
    assert!(
        !status.success(),
        "partial spawn failure must exit non-zero: {status}\n{stdout}"
    );
    assert!(stdout.contains("WORKER name=good pid="), "{stdout}");
    // group_drained=true 是升级式回收对「整组含孙进程已排空」的回执主张
    // (其 killpg→ESRCH 语义由 nlos-process managed_env 套件独立证明);
    // 本 e2e 证明接线:失败路径确实跑了 teardown 并给出回执。
    assert!(
        stdout.contains("KILL name=good outcome=Terminated escalated=false group_drained=true")
            || stdout.contains(
                "KILL name=good outcome=AlreadyTerminated escalated=false group_drained=true"
            ),
        "{stdout}"
    );
    assert!(
        stdout.contains("STOPPED service=system_control"),
        "{stdout}"
    );

    // 直子 pid 取自 WORKER 行(清单顺序保证先于 unit 2 失败点打印,
    // 结构性无竞态);进程已整体退出,孙进程不持管道,收集不会滞留。
    let pid: u32 = stdout
        .lines()
        .find(|line| line.starts_with("WORKER name=good pid="))
        .and_then(|line| line.split_whitespace().find(|tok| tok.starts_with("pid=")))
        .and_then(|tok| tok.strip_prefix("pid="))
        .and_then(|raw| raw.parse().ok())
        .expect("worker pid from WORKER line");
    assert!(
        pid_gone(pid, 20),
        "worker good pid {pid} leaked after partial-failure reclaim"
    );
}

fn resolve_python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

fn parse_seed32(hex: &str) -> [u8; 32] {
    let trimmed = hex.trim();
    assert_eq!(trimmed.len(), 64);
    let mut out = [0_u8; 32];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&trimmed[2 * idx..2 * idx + 2], 16).expect("valid hex byte");
    }
    out
}

fn run_workers_output(root: &Path, manifest: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nlos-up"))
        .args([
            "workers",
            "--root",
            &root.display().to_string(),
            "--workers-manifest",
            &manifest.display().to_string(),
        ])
        .output()
        .expect("run nlos-up workers")
}

#[test]
fn workers_rejects_forged_and_malformed_slots_fail_closed_zero_residual() {
    let scratch = ScratchDir::new("fgd");
    let root = scratch.0.join("root");

    // 1. 合法 D7 格式但权威中不存在的伪造 32-hex slot (含前置无 slot 单元):
    //    必须在拉起任何单元前于权威处 fail-closed 拒绝,零进程残留。
    let forged_manifest = scratch.0.join("forged.manifest");
    let good_base = scratch.0.join("good");
    let forged_base = scratch.0.join("forged");
    std::fs::write(
        &forged_manifest,
        format!(
            "worker = good /bin/sh {sh} {good}\n\
             worker = forged /bin/sh {sh} {forged}\n\
             slot = forged deadbeefdeadbeefdeadbeefdeadbeef\n",
            sh = fixture("worker_env_dump.sh").display(),
            good = good_base.display(),
            forged = forged_base.display(),
        ),
    )
    .expect("write forged manifest");

    let out = run_workers_output(&root, &forged_manifest);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "forged 32-hex slot must fail closed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("rejected by capability authority"),
        "expected capability authority error in stderr:\n{stderr}"
    );
    assert!(
        !stdout.contains("WORKER name="),
        "no worker may spawn when pre-flight fails:\n{stdout}"
    );
    assert!(
        stdout.contains("STOPPED service=system_control"),
        "embedded daemon must stop cleanly on rejection:\n{stdout}"
    );
    assert!(!good_base.with_extension("pid").exists());
    assert!(!good_base.with_extension("env").exists());
    assert!(!forged_base.with_extension("pid").exists());

    // 2. 非 D7 格式(大写/带分隔符/长度非法)同样 fail-closed 拒绝且零拉起。
    for bad_slot in [
        "0123456789ABCDEF0123456789ABCDEF",
        "01234567-89ab-cdef-0123-456789abcdef",
        "short-slot",
    ] {
        let bad_manifest = scratch.0.join("bad.manifest");
        let bad_base = scratch.0.join("bad");
        std::fs::write(
            &bad_manifest,
            format!(
                "worker = bad /bin/sh {sh} {bad}\n\
                 slot = bad {bad_slot}\n",
                sh = fixture("worker_env_dump.sh").display(),
                bad = bad_base.display(),
            ),
        )
        .expect("write bad manifest");
        let bad_out = run_workers_output(&root, &bad_manifest);
        assert!(
            !bad_out.status.success(),
            "malformed slot {bad_slot:?} must fail"
        );
        assert!(!bad_base.with_extension("pid").exists());
    }
}

#[test]
fn workers_rejects_revoked_capability_slot_fail_closed() {
    let scratch = ScratchDir::new("rev");
    let root = scratch.0.join("root");
    let receipt = bootstrap_trial_root(
        &BootstrapOptions::new()
            .with_root(&root)
            .without_default_keypool_probe(),
    )
    .expect("bootstrap");

    let seed = parse_seed32(&std::fs::read_to_string(&receipt.key_file).expect("read seed"));
    let signing_key = SigningKey::from_bytes(&seed);

    {
        let identity = IdentityAuthority::open(root.join("identity")).expect("open identity");
        let binding = identity
            .inspect_current_principal_binding(receipt.principal_id)
            .expect("inspect operator binding");
        let capabilities =
            CapabilityAuthority::open(root.join("capability")).expect("open capability");
        let cmd = RevokeCapabilityRequest {
            handle: CapabilityHandle {
                capability_id: receipt.operator_root_capability_id,
                generation: Generation::INITIAL,
            },
            revoker_key_id: binding.key_id,
            idempotency_key: IdempotencyKey::from_bytes([0x44; 16]),
            revoked_at_ms: 1,
        };
        capabilities
            .revoke_signed(
                &identity,
                SignedRevokeCapabilityRequest {
                    command: cmd,
                    signer: receipt.principal_id,
                    signature: signing_key.sign(&revoke_command_message(cmd)).to_bytes(),
                },
            )
            .expect("revoke operator root capability");
    }

    let manifest_path = scratch.0.join("revoked.manifest");
    let adapter_base = scratch.0.join("adapter");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter {slot}\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = adapter_base.display(),
            slot = receipt.operator_root_capability_hex,
        ),
    )
    .expect("write revoked manifest");

    let out = run_workers_output(&root, &manifest_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "revoked capability must fail closed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("rejected by capability authority"),
        "expected capability authority rejection in stderr:\n{stderr}"
    );
    assert!(!stdout.contains("WORKER name="), "{stdout}");
    assert!(!adapter_base.with_extension("pid").exists());
}

#[test]
fn workers_rejects_holder_mismatch_capability_slot_fail_closed() {
    let scratch = ScratchDir::new("mis");
    let root = scratch.0.join("root");
    let receipt = bootstrap_trial_root(
        &BootstrapOptions::new()
            .with_root(&root)
            .without_default_keypool_probe(),
    )
    .expect("bootstrap");

    let seed = parse_seed32(&std::fs::read_to_string(&receipt.key_file).expect("read seed"));
    let operator_key = SigningKey::from_bytes(&seed);

    let delegated_hex = {
        let identity = IdentityAuthority::open(root.join("identity")).expect("open identity");
        let operator_binding = identity
            .inspect_current_principal_binding(receipt.principal_id)
            .expect("inspect operator binding");
        let other_key = SigningKey::from_bytes(&[0x5c; 32]);
        let other_binding = match identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [0x51; 32],
                control_domain_policy_digest: [0x52; 32],
                public_key: other_key.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: 4_102_444_800_000,
                idempotency_key: IdempotencyKey::from_bytes([0x53; 16]),
                created_at_ms: 0,
            })
            .expect("other binding")
        {
            BootstrapDecision::Created(b) | BootstrapDecision::Replayed(b) => b,
        };
        let capabilities =
            CapabilityAuthority::open(root.join("capability")).expect("open capability");
        let cmd = DelegateCapabilityRequest {
            parent: CapabilityHandle {
                capability_id: receipt.operator_root_capability_id,
                generation: Generation::INITIAL,
            },
            delegator_key_id: operator_binding.key_id,
            recipient_key_id: other_binding.key_id,
            target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0; 16])),
            rights: CapabilityRights::SEMANTIC_APPEND,
            purpose_digest: None,
            valid_from_ms: 0,
            valid_until_ms: 4_102_444_800_000,
            delegation_depth_remaining: 1,
            call_limit: None,
            idempotency_key: IdempotencyKey::from_bytes([0x54; 16]),
            delegated_at_ms: 1,
        };
        let child = capabilities
            .delegate_signed(
                &identity,
                SignedDelegateCapabilityRequest {
                    command: cmd,
                    signer: receipt.principal_id,
                    signature: operator_key.sign(&delegate_command_message(cmd)).to_bytes(),
                },
            )
            .expect("delegate to other holder")
            .record();
        encode_hex(child.handle.capability_id.as_bytes())
    };

    let manifest_path = scratch.0.join("mismatch.manifest");
    let adapter_base = scratch.0.join("adapter");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter {delegated_hex}\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = adapter_base.display(),
        ),
    )
    .expect("write mismatch manifest");

    let out = run_workers_output(&root, &manifest_path);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "holder-mismatched capability must fail closed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("holder mismatch"),
        "expected holder mismatch in stderr:\n{stderr}"
    );
    assert!(!stdout.contains("WORKER name="), "{stdout}");
    assert!(!adapter_base.with_extension("pid").exists());
}

#[test]
fn workers_restart_on_same_root_after_sigterm_reclaims_and_restarts_cleanly() {
    let scratch = ScratchDir::new("restart");
    let root = scratch.0.join("root");
    let receipt = bootstrap_trial_root(
        &BootstrapOptions::new()
            .with_root(&root)
            .without_default_keypool_probe(),
    )
    .expect("bootstrap");
    let slot = receipt.operator_root_capability_hex.clone();

    let manifest_path = scratch.0.join("workers.manifest");
    std::fs::write(
        &manifest_path,
        format!(
            "worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter {slot}\n\
             worker = brain /bin/sh {sh} {b}\n\
             slot = brain {slot}\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = scratch.0.join("adapter").display(),
            b = scratch.0.join("brain").display(),
        ),
    )
    .expect("write manifest");

    // Run 1: 启动双单元 + 验证三变量白名单 + SIGTERM 回收全树
    let up1 = spawn_nlos_up_workers(&root, &manifest_path);
    let head1 = up1.wait_for_workers(2, 60);
    assert!(workers_are_ready(&head1, 2), "{head1}");
    let run1_pids = assert_dual_sh_whitelist_and_collect_pids(&scratch, &root, &slot);

    let (stdout1, status1) = up1.sigterm_and_collect();
    assert_dual_reclaim_output_and_pids_gone(&stdout1, status1, &run1_pids);
    assert!(stdout1.contains("group_drained=true"), "{stdout1}");
    for unit in ["adapter", "brain"] {
        let base = scratch.0.join(unit);
        for ext in ["env", "pid", "grandpid"] {
            let _ = std::fs::remove_file(base.with_extension(ext));
        }
    }

    // Run 2: 立即使用同一 `--root` 重启 `nlos-up workers`,验证无 socket/lock/进程冲突
    let up2 = spawn_nlos_up_workers(&root, &manifest_path);
    let head2 = up2.wait_for_workers(2, 60);
    assert!(workers_are_ready(&head2, 2), "{head2}");
    assert!(
        head2.contains(&format!("operator_root_capability={slot}")),
        "{head2}"
    );
    let run2_pids = assert_dual_sh_whitelist_and_collect_pids(&scratch, &root, &slot);
    for (idx, (unit, pid2, _)) in run2_pids.iter().enumerate() {
        assert_ne!(
            *pid2, run1_pids[idx].1,
            "run2 {unit} must spawn a fresh process"
        );
    }

    let (stdout2, status2) = up2.sigterm_and_collect();
    assert_dual_reclaim_output_and_pids_gone(&stdout2, status2, &run2_pids);
    assert!(stdout2.contains("group_drained=true"), "{stdout2}");
}
