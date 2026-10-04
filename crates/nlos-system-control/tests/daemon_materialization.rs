//! Production materialization-drive tests (`daemon` feature, W54-2): the
//! daemon's [`MaterializationDriver`] powers the W31-F scheduler under the
//! real assembly — declared nodes advance through the W31-A gate
//! (`ELIGIBLE` → `WAITING_RESOURCE` → `MATERIALIZING`) with the Task
//! authority's admission consult wired in, capacity pressure denies
//! without mischief (nodes durably `WAITING_RESOURCE`, window shrunk to
//! the probe floor, the driver still Running), sustained storage failures
//! back off and then fault the thread terminally, and stop/join is
//! idempotent.
//!
//! Platform note: the tests are portable (the daemon derives endpoint
//! paths per OS; the driver-level tests open authorities directly), and
//! the fault-injection test confines the process-global VFS shim to the
//! one authority it opens through it.

#![cfg(feature = "daemon")]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

use nlos_plan::{
    ApplyPlanRevisionRequest, MaterializationRequestStatus, PlanNodeDeclaration, PlanNodeKind,
    PlanNodeState, SqlitePlanAuthority,
};
use nlos_system_control::daemon::{DaemonOptions, assemble};
use nlos_system_control::materialization_driver::{
    MaterializationDriver, MaterializationDriverConfig, MaterializationDriverState,
};
use nlos_task::{ScaleProfile, SqliteTaskAuthority, TASK_PROFILE_10K};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

static NEXT: AtomicU64 = AtomicU64::new(0);

/// Tier whose working-set dimension is zero: every consult projects
/// `active + 1 > 0` and denies (the capacity-pressure posture).
static ZERO_WORKING_SET: ScaleProfile = ScaleProfile {
    profile_id: "task-matdrive-zero-working-set",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 0,
    reclaim_threshold_ratio: None,
};

/// The one authority the fault-injection test opens through the shim VFS.
static VFS_NAME: &str = "nlos-matdrive-fault-vfs";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = now_nanos();
        let path = std::env::temp_dir().join(format!(
            "nlos-matdrive-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&path).expect("create fixture directory");
        Self(path)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn now_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos()
}

/// Endpoint path in the host OS form (Unix socket file on Unix, local
/// pipe name on Windows), mirroring the daemon-assembly harness.
fn endpoint_path(label: &str, role: &str) -> PathBuf {
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    if cfg!(unix) {
        std::env::temp_dir().join(format!(
            "nlos-matdrive-{label}-{role}-{}-{sequence}.sock",
            std::process::id(),
        ))
    } else {
        PathBuf::from(format!(
            r"\\.\pipe\nlos-matdrive-{label}-{role}-{}-{sequence}",
            std::process::id(),
        ))
    }
}

fn node(key: u8, payload: u8, dependency: Option<u8>) -> PlanNodeDeclaration {
    PlanNodeDeclaration {
        node_key: [key; 16],
        kind: PlanNodeKind::AgentRole,
        binding_digest: [payload; 32],
        dependency_keys: dependency
            .map(|dependency| vec![[dependency; 16]])
            .unwrap_or_default(),
        input_selectors_digest: [payload; 32],
        output_contract_digest: [payload; 32],
        policy_digest: [payload; 32],
        resource_ceiling_digest: [payload; 32],
        conditions: None,
    }
}

/// Seeds one plan under the daemon-root-shaped `<root>/plans.sqlite3`
/// (node `0x0a` independent, node `0x0b` depending on it) and drops the
/// handle so the daemon reopens the durable state, mirroring a restart
/// between declaration and drive.
fn seed_plan(root: &TempRoot) -> TaskPlanId {
    let authority =
        SqlitePlanAuthority::open(root.0.join("plans.sqlite3")).expect("open plan authority");
    let plan_id = authority
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x0a, 0x01, None), node(0x0b, 0x02, Some(0x0a))],
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply revision")
        .receipt()
        .plan_id;
    drop(authority);
    plan_id
}

fn node_id_of(authority: &SqlitePlanAuthority, plan_id: TaskPlanId, key: u8) -> TaskNodeId {
    authority
        .list_plan_nodes(plan_id)
        .expect("list nodes")
        .into_iter()
        .find(|record| record.node_key == [key; 16])
        .expect("declared node exists")
        .node_id
}

fn node_state(authority: &SqlitePlanAuthority, plan_id: TaskPlanId, key: u8) -> PlanNodeState {
    authority
        .inspect_node(plan_id, node_id_of(authority, plan_id, key))
        .expect("inspect node")
        .expect("node exists")
        .state
}

fn wait_until(description: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition did not become true: {description}"
        );
        sleep(Duration::from_millis(5));
    }
}

/// The daemon-assembled driver turns the plan→materialization→admission
/// chain by itself: the independent node walks the full gate path
/// (`ELIGIBLE` → `WAITING_RESOURCE` → `MATERIALIZING`, proven by the
/// dense voucher sequence), the Task-side consult's answer is durable in
/// the approved request row, the dependent node stays unselected until
/// its dependency completes, later passes never open a second round, and
/// stop/join is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_driver_materializes_declared_nodes_through_the_task_consult() {
    let root = TempRoot::new("progression");
    let plan_id = seed_plan(&root);
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(endpoint_path("progression", "auth"))
            .with_plain_socket(endpoint_path("progression", "plain"))
            .with_driver_config(MaterializationDriverConfig {
                poll_interval: Duration::from_millis(20),
                ..MaterializationDriverConfig::default()
            }),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    // Read-only verification handle beside the daemon (WAL allows the
    // concurrent reader; the daemon keeps driving).
    let verify = SqlitePlanAuthority::open(root.0.join("plans.sqlite3"))
        .expect("reopen plan authority beside the daemon");

    // The independent node materializes; the dependent one cannot (its
    // dependency is not COMPLETED).
    wait_until("node a materializes", || {
        node_state(&verify, plan_id, 0x0a) == PlanNodeState::Materializing
    });
    assert_eq!(node_state(&verify, plan_id, 0x0b), PlanNodeState::Declared);

    // The full gate path is the dense voucher sequence.
    let node_a = node_id_of(&verify, plan_id, 0x0a);
    let vouchers = verify
        .inspect_node_vouchers(plan_id, node_a)
        .expect("node a vouchers");
    let path: Vec<PlanNodeState> = vouchers.iter().map(|v| v.to_state).collect();
    assert_eq!(
        path,
        vec![
            PlanNodeState::Eligible,
            PlanNodeState::WaitingResource,
            PlanNodeState::Materializing,
        ],
        "the gate drove ELIGIBLE -> WAITING_RESOURCE -> MATERIALIZING"
    );

    // The Task-side consult answered: the approved round carries the
    // admission facts only the consult supplies (task-10k profile with
    // the projected counts).
    let history = verify
        .inspect_node_materialization_requests(plan_id, node_a)
        .expect("node a request history");
    assert_eq!(history.len(), 1, "exactly one gate round");
    assert_eq!(history[0].status, MaterializationRequestStatus::Approved);
    let admission = history[0]
        .admission
        .as_ref()
        .expect("approved round carries admission facts");
    assert_eq!(admission.profile_id, TASK_PROFILE_10K.profile_id);
    assert!(admission.projected_task_nodes >= 1);
    assert!(admission.projected_active_working_set >= 1);
    assert!(history[0].approved_voucher_id.is_some());

    // Later passes never open a second round for the materialized node,
    // and the dependent node stays declaratively blocked.
    sleep(Duration::from_millis(100));
    assert_eq!(
        verify
            .inspect_node_materialization_requests(plan_id, node_a)
            .expect("history again")
            .len(),
        1,
        "no second gate round for a materialized node"
    );
    assert_eq!(node_state(&verify, plan_id, 0x0b), PlanNodeState::Declared);

    // The health face reports the live drive (the approval is visible in
    // the same cycle's snapshot; nothing else can approve afterwards).
    wait_until("health reports the approval", || {
        daemon.materialization_health().total_approved == 1
    });
    let health = daemon.materialization_health();
    assert_eq!(health.state, MaterializationDriverState::Running);
    assert_eq!(health.plans_driven, 1);
    assert_eq!(health.total_approved, 1);
    assert_eq!(health.total_rejected, 0);
    assert_eq!(health.consecutive_failed_cycles, 0);
    assert_eq!(
        health.seats_in_use, 1,
        "the materialized node holds its seat"
    );
    assert_eq!(health.window, MaterializationDriverConfig::default().window);

    // Stop/join is idempotent; the recovery worker retires the same way.
    daemon.stop_materialization();
    assert_eq!(
        daemon.materialization_health().state,
        MaterializationDriverState::Stopped
    );
    daemon.stop_materialization();
    daemon.stop_worker();
    drop(endpoints);
    drop(daemon);
}

/// Capacity pressure denies without mischief: under a Task tier whose
/// working-set dimension is full, every consult denies, every node lands
/// durably `WAITING_RESOURCE` with a rejected probe round (the durable
/// pressure trail), the window shrinks to the floor, and the driver keeps
/// Running — denials are decisions, not failures.
#[test]
fn capacity_pressure_denies_without_mischief() {
    let root = TempRoot::new("capacity");
    let plans = Arc::new(
        SqlitePlanAuthority::open(root.0.join("plans.sqlite3")).expect("open plan authority"),
    );
    let plan_id = plans
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x0a, 0x01, None), node(0x0b, 0x02, None)],
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            applied_at_ms: 1_000,
        })
        .expect("apply revision")
        .receipt()
        .plan_id;
    let tasks = Arc::new(
        SqliteTaskAuthority::open_with_scale_profile(
            root.0.join("tasks.sqlite3"),
            &ZERO_WORKING_SET,
        )
        .expect("open task authority"),
    );
    let mut driver = MaterializationDriver::start(
        Arc::clone(&plans),
        Arc::clone(&tasks),
        MaterializationDriverConfig {
            window: 4,
            poll_interval: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            failure_threshold: 8,
        },
    )
    .expect("start driver");

    wait_until("both nodes durably rejected", || {
        node_state(&plans, plan_id, 0x0a) == PlanNodeState::WaitingResource
            && node_state(&plans, plan_id, 0x0b) == PlanNodeState::WaitingResource
    });
    wait_until("window shrank to the probe floor", || {
        driver.health().window == 1
    });

    // The durable probe trail: every rejected round stays readable (the
    // floor keeps one probe per pass, so the trail grows), and every
    // round carries the typed working-set denial of the zero tier.
    for key in [0x0a_u8, 0x0b] {
        let node_id = node_id_of(&plans, plan_id, key);
        let history = plans
            .inspect_node_materialization_requests(plan_id, node_id)
            .expect("history");
        assert!(!history.is_empty(), "the pressure is a durable probe");
        assert!(history.iter().all(|record| {
            record.status == MaterializationRequestStatus::Rejected
                && matches!(
                    record.rejection.as_ref(),
                    Some(nlos_plan::MaterializationRejection::WorkingSetFull { profile_id, .. })
                        if profile_id == ZERO_WORKING_SET.profile_id
                )
        }));
    }

    let health = driver.health();
    assert_eq!(health.state, MaterializationDriverState::Running);
    assert!(health.total_rejected >= 2, "both initial selections denied");
    assert_eq!(health.total_approved, 0);
    assert_eq!(
        health.consecutive_failed_cycles, 0,
        "denials are not failures"
    );

    driver.stop();
    assert_eq!(driver.health().state, MaterializationDriverState::Stopped);
}

/// Sustained storage failures back off and then fault the thread
/// terminally (never a panic): every write fails through the fault VFS,
/// the consecutive-failure budget exhausts, and the health face reports
/// `Faulted` with the last failure recorded. Reads still pass, so the
/// failure posture is attributed to the write path the gate needs.
#[test]
fn sustained_storage_failures_fault_the_driver() {
    let root = TempRoot::new("fault");
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    let plans = Arc::new(
        SqlitePlanAuthority::open_with_vfs(root.0.join("plans.sqlite3"), Some(VFS_NAME))
            .expect("open plan authority under fault vfs"),
    );
    plans
        .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
            plan_id: None,
            nodes: vec![node(0x0a, 0x01, None)],
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            applied_at_ms: 1_000,
        })
        .expect("seed one node with faults disarmed");
    let tasks = Arc::new(
        SqliteTaskAuthority::open(root.0.join("tasks.sqlite3")).expect("open task authority"),
    );

    nlos_store_fault::arm(nlos_store_fault::FaultMode::FailWritesAfter {
        remaining: 0,
        code: nlos_store_fault::FaultCode::IoErr,
    });
    let mut driver = MaterializationDriver::start(
        Arc::clone(&plans),
        Arc::clone(&tasks),
        MaterializationDriverConfig {
            window: 2,
            poll_interval: Duration::from_millis(5),
            max_backoff: Duration::from_millis(20),
            failure_threshold: 3,
        },
    )
    .expect("start driver under faults");

    wait_until("driver faults after the exhausted budget", || {
        driver.health().state == MaterializationDriverState::Faulted
    });
    let health = driver.health();
    assert_eq!(health.consecutive_failed_cycles, 3);
    assert!(health.last_failure.is_some(), "the fault is diagnosable");
    assert_eq!(health.retry_delay, None);
    // Nothing materialized: the node stays declaratively un-gated.
    let plan_id = plans.list_plan_ids().expect("list plans")[0];
    assert_eq!(node_state(&plans, plan_id, 0x0a), PlanNodeState::Declared);

    nlos_store_fault::disarm();
    driver.stop();
    assert_eq!(
        driver.health().state,
        MaterializationDriverState::Faulted,
        "Faulted is terminal; only a fresh driver restarts the drive"
    );
}
