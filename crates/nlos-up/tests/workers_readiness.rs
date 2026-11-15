//! Shared test helper for the managed-worker readiness boundary.

#[must_use]
pub fn workers_are_ready(output: &str, expected_workers: usize) -> bool {
    // The daemon announces its own readiness before any worker is spawned.
    // Only this later, count-specific line closes the worker startup boundary.
    let ready = format!("READY service=system_control workers={expected_workers}");
    output.lines().any(|line| line == ready)
}

#[test]
fn worker_readiness_rejects_daemon_ready_and_partial_spawn() {
    // Replays the boundary observed in run 43530982662: the daemon is ready,
    // but its second worker has not been reported yet.
    let partial = "WORKERS manifest=/tmp/workers.manifest units=2\n\
                   READY service=system_control root=/tmp/root auth_socket=/tmp/auth.sock\n\
                   WORKER name=adapter pid=15769 status=spawned\n";
    assert!(!workers_are_ready(partial, 2));
    assert!(!workers_are_ready(
        "READY service=system_control workers=1\n",
        2
    ));
    assert!(!workers_are_ready(
        "READY service=system_control workers=20\n",
        2
    ));
}

#[test]
fn worker_readiness_accepts_complete_spawn_for_requested_count() {
    let complete = "WORKER name=adapter pid=11 status=spawned\n\
                    WORKER name=brain pid=12 status=spawned\n\
                    READY service=system_control workers=2\n";
    assert!(workers_are_ready(complete, 2));
    assert!(workers_are_ready(
        "READY service=system_control workers=1\n",
        1
    ));
}
