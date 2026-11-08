//! W82-C 端到端:真实 `nlos-up workers` 二进制 + 双单元清单 + 三变量
//! 白名单 + SIGTERM 主进程 → 子进程树(含孙)干净回收。unix 门控。
#![cfg(unix)]
// 测试体逐字取自任务书(W82-C Task 4 brief);样式 lint 就地豁免,不动断言。
#![allow(clippy::collapsible_if, clippy::map_unwrap_or)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
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
    fn wait_for_line(&self, needle: &str, seconds: u32) -> String {
        for _ in 0..(seconds * 20) {
            if let Ok(buf) = self.stdout.lock() {
                if buf.contains(needle) {
                    return buf.clone();
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "nlos-up workers never printed {needle:?}:\n{}",
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
    spawn_nlos_up_workers_with_path(root, manifest, None)
}

/// `path_override`:替换子进程 PATH(部分失败测试用它把 unit 2 裸名解释
/// 器的 PATH 扫描失败点确定性推后,让 unit 1 夹具先写全证据文件)。
fn spawn_nlos_up_workers_with_path(
    root: &Path,
    manifest: &Path,
    path_override: Option<String>,
) -> RunningUp {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nlos-up"));
    command.args([
        "workers",
        "--root",
        &root.display().to_string(),
        "--workers-manifest",
        &manifest.display().to_string(),
    ]);
    if let Some(path) = path_override {
        command.env("PATH", path);
    }
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

#[test]
fn workers_launch_env_whitelist_and_sigterm_reclaims_tree() {
    let scratch = ScratchDir::new("e2e");
    let root = scratch.0.join("root");
    let manifest_path = scratch.0.join("workers.manifest");
    let dump_base = |name: &str| scratch.0.join(name);

    std::fs::write(
        &manifest_path,
        format!(
            "# W82-C e2e dual-unit manifest\n\
             worker = adapter /bin/sh {sh} {a}\n\
             slot = adapter adapter-slot-token\n\
             worker = brain /bin/sh {sh} {b}\n\
             slot = brain brain-slot-token\n",
            sh = fixture("worker_env_dump.sh").display(),
            a = dump_base("adapter").display(),
            b = dump_base("brain").display(),
        ),
    )
    .expect("write manifest");

    let up = spawn_nlos_up_workers(&root, &manifest_path);
    let stdout_head = up.wait_for_line("READY service=system_control", 60);
    assert!(stdout_head.contains("WORKERS manifest="), "{stdout_head}");
    assert!(stdout_head.contains("units=2"), "{stdout_head}");
    assert!(
        stdout_head.contains("WORKER name=adapter pid="),
        "{stdout_head}"
    );
    assert!(
        stdout_head.contains("WORKER name=brain pid="),
        "{stdout_head}"
    );

    for (unit, slot) in [
        ("adapter", "adapter-slot-token"),
        ("brain", "brain-slot-token"),
    ] {
        let dump = poll_file(&dump_base(unit).with_extension("env"), 30)
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
    }

    // 主进程退出(SIGTERM)→ 子进程树(直子 + 孙)回收
    let (stdout, status) = up.sigterm_and_collect();
    assert!(stdout.contains("KILL name=adapter outcome="), "{stdout}");
    assert!(stdout.contains("KILL name=brain outcome="), "{stdout}");
    assert!(
        stdout.contains("STOPPED service=system_control"),
        "{stdout}"
    );
    assert!(status.success(), "exit {:?}\n{stdout}", status.code());

    for unit in ["adapter", "brain"] {
        let pid: u32 = poll_file(&dump_base(unit).with_extension("pid"), 5)
            .unwrap_or_else(|| panic!("{unit} pid file missing"))
            .trim()
            .parse()
            .unwrap();
        let grandpid: u32 = poll_file(&dump_base(unit).with_extension("grandpid"), 5)
            .unwrap_or_else(|| panic!("{unit} grandpid file missing"))
            .trim()
            .parse()
            .unwrap();
        assert!(
            pid_gone(pid, 20),
            "worker {unit} pid {pid} survived main-exit reclaim"
        );
        assert!(
            pid_gone(grandpid, 20),
            "grandchild of {unit} pid {grandpid} survived main-exit reclaim"
        );
    }
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
    let stdout_head = up.wait_for_line("READY service=system_control", 60);
    assert!(
        stdout_head.contains("WORKER name=pyworker"),
        "{stdout_head}"
    );

    let dump = poll_file(&scratch.0.join("pyw.env"), 30).expect("python env dump");
    let map = parse_env_dump(&dump);
    // PWD/SHLVL/_/LC_CTYPE 为解释器自合成(Task 3 裁定);其余非 LLMOS_* 即泄漏
    let leaked: Vec<&String> = map
        .keys()
        .filter(|key| {
            !key.starts_with("LLMOS_")
                && !matches!(key.as_str(), "PWD" | "SHLVL" | "_" | "LC_CTYPE")
        })
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

    let up = spawn_nlos_up_workers_with_path(&root, &manifest_path, Some(inflated_path()));
    let (stdout, status) = up.wait_and_collect();
    assert!(
        !status.success(),
        "partial spawn failure must exit non-zero: {status}\n{stdout}"
    );
    assert!(stdout.contains("WORKER name=good pid="), "{stdout}");
    assert!(stdout.contains("KILL name=good outcome="), "{stdout}");
    assert!(
        stdout.contains("STOPPED service=system_control"),
        "{stdout}"
    );

    // 直子 pid 取自 WORKER 行(先于失败点打印,天然无竞态);孙进程 pid
    // 只能来自夹具落盘(见 inflated_path 的观察窗偏置说明)。
    let pid: u32 = stdout
        .lines()
        .find(|line| line.starts_with("WORKER name=good pid="))
        .and_then(|line| line.split_whitespace().find(|tok| tok.starts_with("pid=")))
        .and_then(|tok| tok.strip_prefix("pid="))
        .and_then(|raw| raw.parse().ok())
        .expect("worker pid from WORKER line");
    let grandpid: u32 = poll_file(&scratch.0.join("good.grandpid"), 5)
        .unwrap_or_else(|| panic!("good grandpid file missing"))
        .trim()
        .parse()
        .unwrap();
    assert!(
        pid_gone(pid, 20),
        "worker good pid {pid} leaked after partial-failure reclaim"
    );
    assert!(
        pid_gone(grandpid, 20),
        "grandchild of good pid {grandpid} leaked after partial-failure reclaim"
    );
}

/// 膨胀 PATH(全部为不存在目录):unit 1 解释器为绝对路径不受影响;
/// unit 2 裸名解释器须扫完全部条目才判定失败——失败点被确定性推后,
/// 让 unit 1 的夹具先落盘 pid/grandpid 证据(否则升级式回收的 SIGTERM
/// 可能先于夹具写文件到达,属测试观察窗问题而非回收缺陷)。
fn inflated_path() -> String {
    let pad = (0..20_000)
        .map(|index| format!("/tmp/w82c-nopath-{index}"))
        .collect::<Vec<_>>()
        .join(":");
    format!("{pad}:{}", std::env::var("PATH").unwrap_or_default())
}

fn resolve_python3() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("python3"))
        .find(|candidate| candidate.is_file())
}
