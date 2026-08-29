//! Test harness: isolated temp directories, kill-9 child processes with
//! pipe-synchronized `READY` markers (same discipline as
//! `nlos-store/tests/fault_crash.rs`), and the process-global fault-state
//! lock plus a disarm-on-drop guard.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_store_fault::{FaultMode, disarm};

/// Name this test binary registers the fault VFS under.
pub(crate) const VFS_NAME: &str = "nlos-fs-semantics-fault";

/// Environment variable selecting the child role of
/// `fs_semantics_child`.
pub(crate) const CHILD_ROLE_ENV: &str = "NLOS_FS_CHILD_ROLE";
/// Environment variable carrying the child's database path.
pub(crate) const CHILD_DB_ENV: &str = "NLOS_FS_CHILD_DB";
/// The child entry test, invoked via `--exact`.
pub(crate) const CHILD_TEST_NAME: &str = "fs_semantics_child";

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Serializes fault-VFS tests: the injected fault state is process-global.
pub(crate) fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Arms the fault state machine and guarantees disarm on scope exit, so a
/// panicking test cannot poison later tests with a still-armed VFS.
pub(crate) struct ArmedGuard;

pub(crate) fn arm(mode: FaultMode) -> ArmedGuard {
    nlos_store_fault::arm(mode);
    ArmedGuard
}

impl Drop for ArmedGuard {
    fn drop(&mut self) {
        disarm();
    }
}

/// Private per-test directory under the system temp dir, removed on drop
/// (best effort).
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    pub(crate) fn new(tag: &str) -> TempDir {
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-fs-semantics-{}-{}-{tag}",
            std::process::id(),
            seq
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Spawns this same test binary as a child running `fs_semantics_child`
/// with the given role and database path.
pub(crate) fn spawn_child(role: &str, db_path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", CHILD_TEST_NAME, "--nocapture"])
        .env(CHILD_ROLE_ENV, role)
        .env(CHILD_DB_ENV, db_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fs-semantics child")
}

/// Blocks until the child prints its `READY` marker on stdout (pipe
/// synchronization, no sleeps); kills and reaps the child on timeout or
/// early exit.
pub(crate) fn await_ready(child: &mut Child) {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.starts_with("READY") => {
                    let _ = sender.send(Ok(()));
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(Err("child exited without READY".to_string()));
    });
    match receiver.recv_timeout(Duration::from_mins(1)) {
        Ok(Ok(())) => {}
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

/// Force-terminates the child (`SIGKILL` on macOS) and proves it did not
/// exit cleanly.
pub(crate) fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

/// Flushes the `READY` marker to the piped stdout the parent is scanning.
pub(crate) fn print_ready() {
    println!("READY");
    std::io::stdout().flush().expect("flush READY marker");
}
