//! Explicit ROAD-B-004 front-slice scale probe: 10K and 100K durable Task
//! registrations with a lazy permit face. Ignored in the default suite
//! because they materialize real authority databases; run them with
//!
//! ```sh
//! cargo test -p nlos-task --test scale_profile_probe -- --ignored --nocapture
//! ```
//!
//! Measured dimensions (single platform, recorded verbatim in
//! `docs/evidence/stage-b/b-task-scale-001.md`):
//!
//! 1. total wall time to register 10K/100K Tasks through the landed
//!    `register_task` API (fsynced per-registration transactions),
//! 2. per-`request_commit_permit` latency on a 100-Task baseline database
//!    versus the scale database (identical request shape, identical task
//!    IDs) — the laziness assertion: per-permit cost must stay within a
//!    small constant factor, proving the CAS touches only the target task's
//!    indexed rows (`tasks.task_id` PK, `UNIQUE(task_id, idempotency_key)`,
//!    `commit_permits_single_active` partial index) instead of scanning the
//!    task population,
//! 3. the active working set of the published
//!    [`nlos_task::TASK_PROFILE_10K`] / [`nlos_task::TASK_PROFILE_100K`]
//!    tiers,
//! 4. process RSS before/after and database bytes on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_task::{
    AttemptSpec, Authorities, PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority,
    TASK_PROFILE_10K, TASK_PROFILE_100K, TaskSpec, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

const TASK_NODE_COUNT: u64 = 10_000;
const BASELINE_COUNT: u64 = 100;
const LAZY_SAMPLE: u64 = 64;
const ACTIVE_WORKING_SET: u64 = TASK_PROFILE_10K.max_active_working_set;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-scale-probe-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
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

fn file_size(path: &Path) -> u64 {
    fs::metadata(path).map_or(0, |metadata| metadata.len())
}

/// Peak RSS in bytes, measured where a portable read exists (macOS `ps`).
/// Other targets report `None` and the profile line records the gap
/// honestly instead of fabricating a number.
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

fn task_id(index: u64) -> TaskId {
    TaskId::from_bytes(id_bytes(0x01, index))
}

fn attempt_id(index: u64) -> TaskAttemptId {
    TaskAttemptId::from_bytes(id_bytes(0x02, index))
}

fn register_task(authority: &SqliteTaskAuthority, index: u64) {
    authority
        .register_task(TaskSpec {
            task_id: task_id(index),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
}

fn snapshot(index: u64) -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(id_bytes(0x10, index)),
        snapshot_digest: [0x20; 32],
        expected_head_commit_seq: 0,
        effect_history_root: empty_effect_history_root(),
        retry_fence_epoch: 0,
    }
}

fn attempt_spec(index: u64) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: Generation::INITIAL,
        snapshot: snapshot(index),
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(0xc0, index)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa0, index)),
        registered_at_ms: 2_000,
    }
}

fn permit_request(index: u64, key_seed: u8) -> PermitRequest {
    PermitRequest {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: Generation::INITIAL,
        write_set_root: [key_seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xb0, u64::from(key_seed))),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

/// Registers `count` tasks and attempts for the first `sample` of them,
/// then issues one permit per sampled task, timing each request
/// individually. Returns the sorted per-permit durations.
fn permit_latency_sample(
    authority: &SqliteTaskAuthority,
    sample: u64,
    key_seed: u8,
) -> Vec<Duration> {
    for index in 0..sample {
        authority
            .register_attempt(attempt_spec(index))
            .expect("register attempt");
    }
    let mut durations = Vec::with_capacity(usize::try_from(sample).expect("sample fits usize"));
    for index in 0..sample {
        let started = Instant::now();
        let decision = authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_request(index, key_seed),
            )
            .expect("request permit");
        durations.push(started.elapsed());
        assert!(matches!(decision, PermitDecision::Issued(_)));
    }
    durations.sort_unstable();
    durations
}

fn percentile(sorted: &[Duration], per_myriad: u32) -> Duration {
    let denominator = u64::from(u32::try_from(sorted.len()).expect("sample fits u32"));
    let position = (denominator * u64::from(per_myriad))
        .div_ceil(10_000)
        .clamp(1, denominator);
    sorted[usize::try_from(position - 1).expect("position fits usize")]
}

#[test]
#[ignore = "explicit ROAD-B-004 front-slice 10K Task lazy-permit scale probe"]
#[allow(clippy::too_many_lines)]
fn ten_thousand_task_registrations_keep_the_permit_face_lazy() {
    assert!(TASK_PROFILE_10K.admits_task_nodes(TASK_NODE_COUNT));
    assert!(TASK_PROFILE_10K.admits_active_working_set(ACTIVE_WORKING_SET));

    // -- Baseline database: identical request shape over 100 tasks. --------
    let baseline_database = TestDatabase::new("baseline-100");
    let baseline = baseline_database.open();
    for index in 0..BASELINE_COUNT {
        register_task(&baseline, index);
    }
    let baseline_latencies = permit_latency_sample(&baseline, LAZY_SAMPLE, 0x30);
    drop(baseline);

    // -- Scale database: 10K registered Tasks through the landed API. ------
    let rss_before = sample_rss_bytes();
    let scale_database = TestDatabase::new("metadata-10k");
    let authority = scale_database.open();

    let registration_started = Instant::now();
    for index in 0..TASK_NODE_COUNT {
        register_task(&authority, index);
    }
    let registration_elapsed = registration_started.elapsed();

    // Laziness assertion, relative face: the same 64 permit requests on the
    // 10K database must stay within a small constant factor of the baseline
    // (a full task-table scan per request would cost ~100x here). Absolute
    // face: generous per-request ceiling catches pathological regressions.
    let scale_latencies = permit_latency_sample(&authority, LAZY_SAMPLE, 0x30);
    let baseline_p95 = percentile(&baseline_latencies, 9_500);
    let scale_p95 = percentile(&scale_latencies, 9_500);
    assert!(
        scale_p95 <= baseline_p95.saturating_mul(16),
        "permit p95 regressed with task population: baseline {baseline_p95:?}, 10K {scale_p95:?}"
    );
    assert!(
        scale_p95 < Duration::from_millis(100),
        "permit p95 at 10K tasks: {scale_p95:?}"
    );

    // Active working set: 512 outstanding permits across scattered tasks
    // (the TASK_PROFILE_10K tier). Issuance cost must stay O(active), not
    // O(logical); total is asserted against a generous ceiling only.
    for index in LAZY_SAMPLE..ACTIVE_WORKING_SET {
        authority
            .register_attempt(attempt_spec(index))
            .expect("register attempt");
    }
    let working_set_started = Instant::now();
    let mut working_set_latencies =
        Vec::with_capacity(usize::try_from(ACTIVE_WORKING_SET).expect("fits usize"));
    for index in LAZY_SAMPLE..ACTIVE_WORKING_SET {
        let started = Instant::now();
        let decision = authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_request(index, 0x40),
            )
            .expect("working-set permit");
        working_set_latencies.push(started.elapsed());
        assert!(matches!(decision, PermitDecision::Issued(_)));
    }
    let working_set_elapsed = working_set_started.elapsed();
    working_set_latencies.sort_unstable();
    assert!(
        working_set_elapsed < Duration::from_mins(2),
        "working set: {working_set_elapsed:?}"
    );

    // Point reads across the whole ID space stay key-scoped.
    let inspect_started = Instant::now();
    for index in [0, 1, TASK_NODE_COUNT / 2, TASK_NODE_COUNT - 1] {
        let record = authority
            .inspect_task(task_id(index))
            .expect("inspect task");
        assert_eq!(record.task_id, task_id(index));
    }
    let inspect_elapsed = inspect_started.elapsed();

    let rss_after = sample_rss_bytes();
    drop(authority);
    let database_bytes = file_size(&scale_database.path);

    eprintln!(
        "10K task profile (single platform): registrations={TASK_NODE_COUNT} \
         register_total={registration_elapsed:?} \
         register_mean={:?} \
         permit_p50_100={:?} permit_p95_100={baseline_p95:?} permit_max_100={:?} \
         permit_p50_10k={:?} permit_p95_10k={scale_p95:?} permit_max_10k={:?} \
         working_set={ACTIVE_WORKING_SET} working_set_total={working_set_elapsed:?} \
         working_set_p50={:?} working_set_p95={:?} \
         inspect4={inspect_elapsed:?} database_bytes={database_bytes} \
         rss_before={rss_before:?} rss_after={rss_after:?}",
        registration_elapsed / 1_000,
        baseline_latencies[baseline_latencies.len() / 2],
        baseline_latencies[baseline_latencies.len() - 1],
        scale_latencies[scale_latencies.len() / 2],
        scale_latencies[scale_latencies.len() - 1],
        working_set_latencies[working_set_latencies.len() / 2],
        percentile(&working_set_latencies, 9_500),
    );
}

const TASK_NODE_COUNT_100K: u64 = 100_000;
const ACTIVE_WORKING_SET_100K: u64 = TASK_PROFILE_100K.max_active_working_set;

#[test]
#[ignore = "explicit ROAD-B-004 front-slice 100K Task lazy-permit scale probe"]
#[allow(clippy::too_many_lines)]
fn one_hundred_thousand_task_registrations_keep_the_permit_face_lazy() {
    assert!(TASK_PROFILE_100K.admits_task_nodes(TASK_NODE_COUNT_100K));
    assert!(TASK_PROFILE_100K.admits_active_working_set(ACTIVE_WORKING_SET_100K));

    // -- Baseline database: identical request shape over 100 tasks. --------
    let baseline_database = TestDatabase::new("baseline-100-100k");
    let baseline = baseline_database.open();
    for index in 0..BASELINE_COUNT {
        register_task(&baseline, index);
    }
    let baseline_latencies = permit_latency_sample(&baseline, LAZY_SAMPLE, 0x50);
    drop(baseline);

    // -- Scale database: 100K registered Tasks through the landed API. ------
    let rss_before = sample_rss_bytes();
    let scale_database = TestDatabase::new("metadata-100k");
    let authority = scale_database.open();

    let registration_started = Instant::now();
    for index in 0..TASK_NODE_COUNT_100K {
        register_task(&authority, index);
    }
    let registration_elapsed = registration_started.elapsed();

    let scale_latencies = permit_latency_sample(&authority, LAZY_SAMPLE, 0x50);
    let baseline_p95 = percentile(&baseline_latencies, 9_500);
    let scale_p95 = percentile(&scale_latencies, 9_500);
    assert!(
        scale_p95 <= baseline_p95.saturating_mul(16),
        "permit p95 regressed with task population: baseline {baseline_p95:?}, 100K {scale_p95:?}"
    );
    assert!(
        scale_p95 < Duration::from_millis(100),
        "permit p95 at 100K tasks: {scale_p95:?}"
    );

    for index in LAZY_SAMPLE..ACTIVE_WORKING_SET_100K {
        authority
            .register_attempt(attempt_spec(index))
            .expect("register attempt");
    }
    let working_set_started = Instant::now();
    let mut working_set_latencies =
        Vec::with_capacity(usize::try_from(ACTIVE_WORKING_SET_100K).expect("fits usize"));
    for index in LAZY_SAMPLE..ACTIVE_WORKING_SET_100K {
        let started = Instant::now();
        let decision = authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_request(index, 0x60),
            )
            .expect("working-set permit");
        working_set_latencies.push(started.elapsed());
        assert!(matches!(decision, PermitDecision::Issued(_)));
    }
    let working_set_elapsed = working_set_started.elapsed();
    working_set_latencies.sort_unstable();
    assert!(
        working_set_elapsed < Duration::from_mins(20),
        "working set: {working_set_elapsed:?}"
    );

    let inspect_started = Instant::now();
    for index in [0, 1, TASK_NODE_COUNT_100K / 2, TASK_NODE_COUNT_100K - 1] {
        let record = authority
            .inspect_task(task_id(index))
            .expect("inspect task");
        assert_eq!(record.task_id, task_id(index));
    }
    let inspect_elapsed = inspect_started.elapsed();

    let rss_after = sample_rss_bytes();
    drop(authority);
    let database_bytes = file_size(&scale_database.path);

    eprintln!(
        "100K task profile (single platform): registrations={TASK_NODE_COUNT_100K} \
         register_total={registration_elapsed:?} \
         register_mean={:?} \
         permit_p50_100={:?} permit_p95_100={baseline_p95:?} permit_max_100={:?} \
         permit_p50_100k={:?} permit_p95_100k={scale_p95:?} permit_max_100k={:?} \
         working_set={ACTIVE_WORKING_SET_100K} working_set_total={working_set_elapsed:?} \
         working_set_p50={:?} working_set_p95={:?} \
         inspect4={inspect_elapsed:?} database_bytes={database_bytes} \
         rss_before={rss_before:?} rss_after={rss_after:?}",
        registration_elapsed / 1_000,
        baseline_latencies[baseline_latencies.len() / 2],
        baseline_latencies[baseline_latencies.len() - 1],
        scale_latencies[scale_latencies.len() / 2],
        scale_latencies[scale_latencies.len() - 1],
        working_set_latencies[working_set_latencies.len() / 2],
        percentile(&working_set_latencies, 9_500),
    );
}
