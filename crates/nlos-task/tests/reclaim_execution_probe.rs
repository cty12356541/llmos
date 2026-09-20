//! Explicit W31-C reclaim real-execution benchmark: fill one published
//! tier's active working set to its hard cap, then drive the
//! advisory-surfaced reclaim execution to completion through
//! [`SqliteTaskAuthority::drive_working_set_reclaim`] and measure the real
//! closure cost. Ignored in the default suite because it materializes real
//! authority databases; run with
//!
//! ```sh
//! cargo test -p nlos-task --test reclaim_execution_probe -- --ignored --nocapture
//! ```
//!
//! Measured dimensions (single platform, recorded verbatim in
//! `docs/evidence/stage-b/b-task-scale-001.md` §W31-C):
//!
//! 1. working-set fill: total wall time and mean issuance latency for the
//!    cap-sized set of permits (fsynced per-issuance transactions),
//! 2. the drive: total wall time, evicted-unit count (must equal the
//!    soft-threshold overshoot exactly), and the resulting eviction
//!    throughput,
//! 3. durable bytes before and after the drive (closure receipts and
//!    permit state transitions are the durable footprint of reclaim),
//! 4. process RSS before/after where a portable read exists.
//!
//! The probe's working set is uniform (every member an evictable
//! no-effect-slot permit); mixed-state selectivity is pinned by the
//! default-suite `tests/reclaim_execution.rs`, not re-measured here.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_task::{
    Authorities, PermitDecision, PermitRequest, ScaleProfile, SnapshotBundle, SqliteTaskAuthority,
    WorkingSetReclaimExecutionRequest, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-reclaim-probe-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open_with_profile(&self, profile: &'static ScaleProfile) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open_with_scale_profile(&self.path, profile)
            .expect("open task authority with profile")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn database_bytes(path: &Path) -> u64 {
    ["", "-wal"]
        .into_iter()
        .map(|suffix| {
            let mut value = path.as_os_str().to_os_string();
            value.push(suffix);
            fs::metadata(PathBuf::from(value)).map_or(0, |metadata| metadata.len())
        })
        .sum()
}

#[cfg(target_os = "macos")]
fn sample_rss_bytes() -> Option<u64> {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let kilobytes: u64 = text.trim().parse().ok()?;
    Some(kilobytes * 1024)
}

#[cfg(not(target_os = "macos"))]
fn sample_rss_bytes() -> Option<u64> {
    None
}

fn id_bytes(domain: u8, index: u64) -> [u8; 16] {
    let mut bytes = [domain; 16];
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    bytes
}

fn register_task(authority: &SqliteTaskAuthority, index: u64) {
    authority
        .register_task(nlos_task::TaskSpec {
            task_id: TaskId::from_bytes(id_bytes(0x01, index)),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .expect("register task");
}

fn register_attempt(authority: &SqliteTaskAuthority, index: u64) {
    authority
        .register_attempt(nlos_task::AttemptSpec {
            task_id: TaskId::from_bytes(id_bytes(0x01, index)),
            attempt_id: TaskAttemptId::from_bytes(id_bytes(0x02, index)),
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes(id_bytes(0x10, index)),
                snapshot_digest: [0x20; 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(0xc0, index)),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa0, index)),
            registered_at_ms: 2_000,
        })
        .expect("register attempt");
}

fn permit_request(index: u64, requested_at_ms: i64) -> PermitRequest {
    PermitRequest {
        task_id: TaskId::from_bytes(id_bytes(0x01, index)),
        attempt_id: TaskAttemptId::from_bytes(id_bytes(0x02, index)),
        attempt_generation: Generation::INITIAL,
        write_set_root: [0x33; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xb0, index)),
        valid_until_ms: 99_999,
        requested_at_ms,
    }
}

/// Fills the tier's full active working set, timing each issuance. Returns
/// the cap issuance's reclaim-execution warrant (projected count equals the
/// hard cap, squarely inside the advisory zone).
fn fill_working_set(
    authority: &SqliteTaskAuthority,
    cap: u64,
) -> (
    Duration,
    Vec<Duration>,
    nlos_task::WorkingSetReclaimExecution,
) {
    let mut latencies = Vec::new();
    let mut warrant = None;
    let started = Instant::now();
    for index in 0..cap {
        register_task(authority, index);
        register_attempt(authority, index);
        let issuance_started = Instant::now();
        let decision = authority
            .request_commit_permit_decision_with_authorities_struct(
                Authorities::default(),
                permit_request(
                    index,
                    3_000 + 100 * i64::try_from(index).expect("index fits"),
                ),
            )
            .expect("working-set permit");
        latencies.push(issuance_started.elapsed());
        assert!(matches!(decision.permit, PermitDecision::Issued(_)));
        if decision.reclaim_execution.is_some() {
            warrant = decision.reclaim_execution;
        }
    }
    (
        started.elapsed(),
        latencies,
        warrant.expect("cap-sized issuance must surface the reclaim warrant"),
    )
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::cast_precision_loss)] // Throughput display only: eviction count into f64 seconds.
fn run_tier(label: &str, profile: &'static ScaleProfile, ceiling: Duration) {
    let cap = profile.max_active_working_set;
    let threshold = profile.reclaim_threshold_count();
    let overshoot = cap - threshold;
    assert!(profile.admits_active_working_set(cap));
    assert!(overshoot > 0, "tier must leave an advisory zone to reclaim");

    let database = TestDatabase::new(label);
    let authority = database.open_with_profile(profile);
    let rss_before = sample_rss_bytes();

    let (fill_total, mut fill_latencies, warrant) = fill_working_set(&authority, cap);
    fill_latencies.sort_unstable();
    let fill_p95 = fill_latencies[fill_latencies.len() - fill_latencies.len() / 20];
    let pressure = authority
        .inspect_working_set_pressure()
        .expect("pressure at cap");
    assert_eq!(pressure.active_count, cap);
    assert!(pressure.needs_reclaim);

    let bytes_before_drive = database_bytes(&database.path);
    let drive_started = Instant::now();
    let report = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: warrant,
            executed_at_ms: 900_000,
        })
        .expect("drive reclaim execution");
    let drive_total = drive_started.elapsed();

    assert_eq!(
        report.evictions.len() as u64,
        overshoot,
        "drive must evict exactly the soft-threshold overshoot"
    );
    assert_eq!(report.pre_active_count, cap);
    assert_eq!(report.post_active_count, threshold);
    assert_eq!(report.reclaim_threshold_count, threshold);
    assert!(report.pressure_relieved);
    assert!(
        drive_total < ceiling,
        "{label} drive: {drive_total:?} exceeded ceiling {ceiling:?}"
    );

    let relieved = authority
        .inspect_working_set_pressure()
        .expect("pressure after drive");
    assert_eq!(relieved.active_count, threshold);
    assert!(!relieved.needs_reclaim);

    let bytes_after_drive = database_bytes(&database.path);
    let rss_after = sample_rss_bytes();
    drop(authority);

    let eviction_per_second = overshoot as f64 / drive_total.as_secs_f64();
    eprintln!(
        "reclaim execution benchmark ({label}, single platform): cap={cap} threshold={threshold} \
         fill_total={fill_total:?} fill_p50={:?} fill_p95={fill_p95:?} \
         evicted={overshoot} drive_total={drive_total:?} \
         evictions_per_second={eviction_per_second:.1} \
         durable_bytes_before_drive={bytes_before_drive} \
         durable_bytes_after_drive={bytes_after_drive} \
         durable_bytes_delta={} rss_before={rss_before:?} rss_after={rss_after:?}",
        fill_latencies[fill_latencies.len() / 2],
        bytes_after_drive.saturating_sub(bytes_before_drive),
    );
}

#[test]
#[ignore = "explicit W31-C reclaim real-execution benchmark (10K tier working set)"]
fn ten_k_tier_reclaim_execution_drives_the_overshoot_closed() {
    run_tier("10k", &nlos_task::TASK_PROFILE_10K, Duration::from_mins(2));
}

#[test]
#[ignore = "explicit W31-C reclaim real-execution benchmark (100K tier working set)"]
fn hundred_k_tier_reclaim_execution_drives_the_overshoot_closed() {
    run_tier(
        "100k",
        &nlos_task::TASK_PROFILE_100K,
        Duration::from_mins(5),
    );
}
