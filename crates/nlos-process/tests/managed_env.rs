//! W82-C 受管三变量白名单注入与升级式组回收。unix 门控(组原语仅 Unix)。
#![cfg(unix)]

use nlos_process::{
    MANAGED_ENV_AUTH_ENDPOINT, MANAGED_ENV_PROCESS_IDENTITY_SEED, MANAGED_ENV_ROOT_CAPABILITY_SLOT,
    ManagedWorkerEnv, ProcessSupervisor, SpawnSupervisedRequest, prepare_managed_command,
    spawn_managed_worker, terminate_group_with_escalation,
};
use nlos_types::{Generation, ProcessId};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn temp_dir(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("w82c-me-{}-{tag}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

fn sample_env() -> ManagedWorkerEnv {
    ManagedWorkerEnv {
        auth_endpoint: "/tmp/w82c-auth.sock".to_owned(),
        process_identity_seed: "/tmp/w82c-operator.key".to_owned(),
        root_capability_slot: "slot-token-xyz".to_owned(),
    }
}

fn poll_file(path: &Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return Some(text);
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

fn pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let ok = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 同一解释器在空环境(`env_clear`)下自合成的变量名集合(基线探测,
/// 构建无关:CI/本机/不同 Python 发行版各自合成什么都由此归一)。
fn python_self_synthesized_env(python: &Path) -> Vec<String> {
    let output = std::process::Command::new(python)
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

fn resolve_python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}

/// 平台事实:macOS /bin/sh(bash-as-sh)启动时自行合成的环境变量
/// (PWD/SHLVL/_)——非父环境继承,验证零泄漏断言时需剔除。
const SHELL_SYNTHESIZED: [&str; 3] = ["PWD", "SHLVL", "_"];

#[test]
fn env_constants_and_entries_order() {
    assert_eq!(MANAGED_ENV_AUTH_ENDPOINT, "LLMOS_AUTH_ENDPOINT");
    assert_eq!(
        MANAGED_ENV_PROCESS_IDENTITY_SEED,
        "LLMOS_PROCESS_IDENTITY_SEED"
    );
    assert_eq!(
        MANAGED_ENV_ROOT_CAPABILITY_SLOT,
        "LLMOS_ROOT_CAPABILITY_SLOT"
    );
    let entries = sample_env().entries();
    let names: Vec<&str> = entries.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            MANAGED_ENV_AUTH_ENDPOINT,
            MANAGED_ENV_PROCESS_IDENTITY_SEED,
            MANAGED_ENV_ROOT_CAPABILITY_SLOT
        ]
    );
    assert!(entries[2].1.contains("slot-token-xyz"));
}

#[test]
fn prepared_command_carries_exactly_three_envs_and_cwd() {
    let cwd = temp_dir("prep");
    let command =
        prepare_managed_command("/bin/sh", &["script.sh".to_owned()], &cwd, &sample_env());
    let mut envs: Vec<(String, String)> = command
        .get_envs()
        .map(|(k, v)| {
            (
                k.to_string_lossy().into_owned(),
                v.map(|b| b.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            )
        })
        .collect();
    envs.sort();
    assert_eq!(envs.len(), 3, "exactly three env vars, got {envs:?}");
    assert_eq!(command.get_current_dir(), Some(cwd.as_path()));
    let _ = std::fs::remove_dir_all(&cwd);
}

#[test]
fn spawned_worker_env_is_exactly_three_vars_and_parent_does_not_leak() {
    let dir = temp_dir("sh");
    let base = dir.join("w");
    let argv = vec![
        fixture("worker_env_dump.sh").display().to_string(),
        base.display().to_string(),
    ];
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([4; 16]);
    let mut spawned = spawn_managed_worker(
        &supervisor,
        SpawnSupervisedRequest {
            process_id,
            process_generation: Generation::INITIAL,
            registered_at_ms: 0,
        },
        "/bin/sh",
        &argv,
        &dir,
        &sample_env(),
    )
    .expect("spawn");
    let dump = poll_file(&base.with_extension("env"), 15).expect("env dump");
    let map = parse_env_dump(&dump);
    // cargo test 父环境必然含 PATH/HOME 等 → 除 shell 自建变量
    // (见 SHELL_SYNTHESIZED,非父环境继承)外集合恰等三键即证父环境零泄漏
    let inherited: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !SHELL_SYNTHESIZED.contains(key))
        .collect();
    assert_eq!(
        inherited,
        [
            "LLMOS_AUTH_ENDPOINT",
            "LLMOS_PROCESS_IDENTITY_SEED",
            "LLMOS_ROOT_CAPABILITY_SLOT"
        ],
        "child env leaked: {map:?}"
    );
    assert_eq!(
        map.get("LLMOS_AUTH_ENDPOINT").unwrap(),
        "/tmp/w82c-auth.sock"
    );
    assert_eq!(
        map.get("LLMOS_ROOT_CAPABILITY_SLOT").unwrap(),
        "slot-token-xyz"
    );
    // 先读 grandpid 再回收:排除"SIGTERM 先于夹具写 grandpid 落地"的竞态
    let grandpid: u32 = poll_file(&base.with_extension("grandpid"), 5)
        .expect("grandpid")
        .trim()
        .parse()
        .unwrap();
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("teardown");
    assert!(receipt.group_drained, "{receipt:?}");
    assert!(pid_gone(grandpid, 10), "grandchild {grandpid} survived");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn python_worker_env_is_exactly_three_vars() {
    let Some(python) = resolve_python3() else {
        panic!(
            "python3 required for the W82-C gate-letter test (acceptance: 拉起外部 Python 组件)"
        );
    };
    let dir = temp_dir("py");
    let base = dir.join("w");
    let argv = vec![
        fixture("worker_env_dump.py").display().to_string(),
        base.display().to_string(),
    ];
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([5; 16]);
    let mut spawned = spawn_managed_worker(
        &supervisor,
        SpawnSupervisedRequest {
            process_id,
            process_generation: Generation::INITIAL,
            registered_at_ms: 0,
        },
        python.to_str().expect("utf8 python path"),
        &argv,
        &dir,
        &sample_env(),
    )
    .expect("spawn");
    let dump = poll_file(&base.with_extension("env"), 20).expect("python env dump");
    let map = parse_env_dump(&dump);
    // 平台事实(CI 实证,2026-11-08):CPython 在 env_clear 的空环境下会自行
    // 合成环境变量,且集合因构建而异——PEP 538 的 LC_CTYPE=C.UTF-8(常见);
    // macOS 系统构建另有 __CF_USER_TEXT_ENCODING(CoreFoundation,按 uid 合成)
    // 与 CPATH/LIBRARY_PATH/MANPATH/SDKROOT(CI macOS 与本机 miniconda 构建
    // 合成集不同,故不枚举)。基线探测:同一解释器在空环境下自合成什么,
    // 受管子进程就允许多什么;集合恰等(基线 ∪ 三 LLMOS_*)仍证明父环境零
    // 泄漏(env_clear 后父变量必须一个不少地缺席)。
    let baseline = python_self_synthesized_env(&python);
    let mut expected: Vec<&str> = baseline.iter().map(String::as_str).collect();
    expected.extend([
        MANAGED_ENV_AUTH_ENDPOINT,
        MANAGED_ENV_PROCESS_IDENTITY_SEED,
        MANAGED_ENV_ROOT_CAPABILITY_SLOT,
    ]);
    expected.sort_unstable();
    let actual: Vec<&str> = map.keys().map(String::as_str).collect();
    assert_eq!(
        actual, expected,
        "python child env differs from empty-env baseline + three LLMOS vars: {map:?}"
    );
    assert!(map.contains_key("LLMOS_PROCESS_IDENTITY_SEED"));
    terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("teardown");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn escalation_forces_term_ignoring_direct_child_and_drains_group() {
    let dir = temp_dir("esc");
    let marker = dir.join("m");
    // 直子 sh 忽略 SIGTERM(trap ''),孙进程 sleep 300 → 宽限超时 → 强杀
    let mut script = Command::new("/bin/sh");
    script.arg("-c").arg(format!(
        "trap '' TERM; sleep 300 & echo $! > {gp}; echo $$ > {pid}; wait",
        gp = marker.with_extension("grandpid").display(),
        pid = marker.with_extension("pid").display(),
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([6; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(
            SpawnSupervisedRequest {
                process_id,
                process_generation: Generation::INITIAL,
                registered_at_ms: 0,
            },
            &mut script,
        )
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .unwrap();
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_millis(800),
    )
    .expect("escalation teardown");
    assert!(
        receipt.escalated,
        "expected SIGKILL escalation: {receipt:?}"
    );
    assert!(receipt.group_drained);
    assert!(receipt.exit_status.is_some());
    assert!(pid_gone(grandpid, 10), "grandchild survived SIGKILL drain");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn already_terminated_direct_child_still_drains_lingering_grandchild() {
    let dir = temp_dir("orphan");
    let marker = dir.join("m");
    let mut script = Command::new("/bin/sh");
    script.arg("-c").arg(format!(
        "sleep 300 & echo $! > {gp}; exit 0",
        gp = marker.with_extension("grandpid").display(),
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([7; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(
            SpawnSupervisedRequest {
                process_id,
                process_generation: Generation::INITIAL,
                registered_at_ms: 0,
            },
            &mut script,
        )
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .unwrap();
    let receipt = terminate_group_with_escalation(
        &supervisor,
        process_id,
        Generation::INITIAL,
        spawned.child(),
        Duration::from_secs(5),
    )
    .expect("orphan drain teardown");
    // 平台语义:组长(直子)已亡、孙进程仍在 → 组仍在 → killpg 成功
    // (Applied)→ 归类 Terminated;AlreadyTerminated 仅当首发组信号时
    // 整组已不存在(ManagedKillOutcome 文档契约)。本测试的核心断言是
    // 直子先亡仍能排空残留孙进程(group_drained + pid_gone)。
    assert_eq!(
        receipt.outcome,
        nlos_process::ManagedKillOutcome::Terminated
    );
    assert!(
        receipt.group_drained,
        "lingering grandchild must be drained: {receipt:?}"
    );
    assert!(pid_gone(grandpid, 10));
    let _ = std::fs::remove_dir_all(&dir);
}
