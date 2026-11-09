//! W82-C 受管分组监管面:KillScope 事实记录、Unix 进程组拉起与组杀。
//! unix 门控:组原语仅 Unix 有真实语义(Windows taskkill /T 既有覆盖)。
#![cfg(unix)]

use nlos_process::{
    KillScope, PlatformKillAdapter, PosixPlatformKillAdapter, ProcessSupervisor,
    RegisterSupervisorPidRequest, SpawnSupervisedRequest, SupervisorError, SupervisorPidRegistry,
    SupervisorPidRegistryError, SupervisorSignalOutcome,
};
use nlos_types::{Generation, ProcessId};
use std::collections::HashMap;
use std::process::Command;

fn spawn_request(process_id: ProcessId) -> SpawnSupervisedRequest {
    SpawnSupervisedRequest {
        process_id,
        process_generation: Generation::INITIAL,
        registered_at_ms: 0,
    }
}

fn sh_sleep_with_grandchild(marker: &str) -> Command {
    // 孙进程:sleep 300(其 pid 写入 marker.grandpid);直子 sh 存活等待。
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "sleep 300 & echo $! > {marker}.grandpid; echo $$ > {marker}.pid; wait"
    ));
    command
}

#[test]
fn registry_records_kill_scope_and_replay_mismatch_fails_closed() {
    let registry = SupervisorPidRegistry::new();
    let process_id = ProcessId::from_bytes([7; 16]);
    let request = || RegisterSupervisorPidRequest {
        process_id,
        process_generation: Generation::INITIAL,
        os_pid: 4242,
        registered_at_ms: 0,
    };
    registry
        .register_at_scope(request(), KillScope::Process)
        .expect("register");
    let entry = registry.lookup(process_id).expect("lookup");
    assert_eq!(entry.kill_scope, KillScope::Process);
    // 同 (id, gen, pid) 但 scope 不同 → fail-closed
    let err = registry
        .register_at_scope(request(), KillScope::ProcessGroup)
        .unwrap_err();
    assert!(
        matches!(err, SupervisorPidRegistryError::KillScopeMismatch { .. }),
        "{err}"
    );
    // 完全一致的 replay 仍通过;经典 register 委托 Process scope 同样 replay 通过
    registry
        .register_at_scope(request(), KillScope::Process)
        .expect("replay");
    registry
        .register(request())
        .expect("classic register replays");
}

#[test]
fn posix_adapter_group_signal_resolves_generation_fence() {
    let process_id = ProcessId::from_bytes([9; 16]);
    let map = HashMap::from([((process_id, Generation::INITIAL), 999_999u32)]);
    let adapter = PosixPlatformKillAdapter::with_generation_pid_map(map);
    // 代际不匹配 → fail closed(不触组)
    let err = adapter
        .signal_platform_kill_group(
            process_id,
            Generation::INITIAL.checked_next().unwrap(),
            false,
        )
        .unwrap_err();
    let _ = err;
    // 未知 (id,gen) 之外的已映射但不存在 pid → killpg ESRCH → AlreadyTerminated
    let outcome = adapter
        .signal_platform_kill_group(process_id, Generation::INITIAL, true)
        .expect("probe");
    assert_eq!(
        outcome,
        nlos_process::PlatformKillAdapterOutcome::AlreadyTerminated
    );
}

#[test]
fn grouped_spawn_makes_child_its_own_group_leader() {
    let dir = std::env::temp_dir().join(format!("w82c-leader-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let marker = dir.join("m");
    // kill -0 -$$ 成功 ⇔ 本进程是组长(pgid == pid)
    let mut command = Command::new("/bin/sh");
    command.arg("-c").arg(format!(
        "if kill -0 -$$ 2>/dev/null; then echo leader > {}; else echo follower > {}; fi; sleep 30",
        marker.display(),
        marker.display()
    ));
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([1; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(spawn_request(process_id), &mut command)
        .expect("grouped spawn");
    assert_eq!(
        supervisor
            .registry()
            .lookup(process_id)
            .expect("entry")
            .kill_scope,
        KillScope::ProcessGroup
    );
    let leader = poll_file(&marker, 10).expect("leader marker");
    assert_eq!(leader.trim(), "leader");
    supervisor
        .kill(process_id, Generation::INITIAL)
        .expect("kill");
    let _ = spawned.child().wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn group_kill_terminates_direct_child_and_grandchild() {
    let dir = std::env::temp_dir().join(format!("w82c-tree-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let marker = dir.join("m");
    let mut command = sh_sleep_with_grandchild(&marker.display().to_string());
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([2; 16]);
    let mut spawned = supervisor
        .spawn_supervised_grouped(spawn_request(process_id), &mut command)
        .expect("grouped spawn");
    let grandpid: u32 = poll_file(&marker.with_extension("grandpid"), 10)
        .expect("grandpid")
        .trim()
        .parse()
        .expect("pid number");
    let outcome = supervisor
        .kill(process_id, Generation::INITIAL)
        .expect("kill");
    assert_eq!(outcome, SupervisorSignalOutcome::Applied);
    let _ = spawned.child().wait();
    assert!(
        wait_pid_gone(grandpid, 50),
        "grandchild {grandpid} survived group kill"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_force_requires_process_group_scope() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([3; 16]);
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut command)
        .expect("plain spawn");
    let err = supervisor
        .kill_force(process_id, Generation::INITIAL)
        .unwrap_err();
    assert!(matches!(err, SupervisorError::PlatformKill(_)), "{err}");
    supervisor
        .kill(process_id, Generation::INITIAL)
        .expect("cleanup kill");
    let _ = spawned.child().wait();
}

fn poll_file(path: &std::path::Path, seconds: u32) -> Option<String> {
    for _ in 0..(seconds * 20) {
        if let Ok(text) = std::fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return Some(text);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    None
}

fn wait_pid_gone(pid: u32, seconds: u32) -> bool {
    for _ in 0..(seconds * 20) {
        let status = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status();
        match status {
            Ok(stat) if stat.success() => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            _ => return true,
        }
    }
    false
}
