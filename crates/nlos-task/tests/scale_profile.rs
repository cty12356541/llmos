//! `ScaleProfile` skeleton acceptance for the ROAD-B-004 front slice.
//!
//! Publishes the first named single-node tier ([`nlos_task::TASK_PROFILE_10K`])
//! and pins the landed **key-scoped query pattern** it is measured against:
//! every registration/permit path reaches rows through primary-key or
//! unique-index lookups (`tasks.task_id` PK, `UNIQUE(task_id, idempotency_key)`
//! on attempts and permits, and the `commit_permits_single_active` partial
//! unique index on `commit_permits(task_id) WHERE permit_state = 0`); no
//! store path scans the whole task table. Timing evidence for the 10K tier
//! (per-permit latency at `N=100` vs `N=10_000`, registration throughput, RSS)
//! lives in the explicit, ignored probe `tests/scale_profile_probe.rs` and
//! `docs/evidence/stage-b/b-task-scale-001.md`; this default-suite test only
//! asserts semantics plus a pathological-slowness guard.
//!
//! Honest mapping: `TaskSpec` carries no plan field, so the ROAD-B-004
//! "logical `TaskNode`" dimension is provisionally carried by durable `Task`
//! registrations; the TaskPlan/TaskNode declaration surface, Dependency
//! Resolver, and checkpoint/rehydrate benchmarks are registered gaps.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_task::{
    AttemptSpec, Authorities, PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority,
    TASK_PROFILE_10K, TaskSpec, WorkingSetPressure, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

const REGISTRATION_COUNT: u64 = 200;
const ACTIVE_SAMPLE: u64 = 16;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-scale-profile-{name}-{}-{sequence}.sqlite3",
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
    let decision = authority
        .register_task(TaskSpec {
            task_id: task_id(index),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    assert_eq!(
        decision,
        nlos_task::TaskRegistrationDecision::Created(task_id(index))
    );
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

fn issued_permit(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued permit, got {other:?}"),
    }
}

#[test]
fn task_10k_tier_is_published_and_registration_face_stays_key_scoped() {
    let database = TestDatabase::new("tier");
    let authority = database.open();

    let registered = Instant::now();
    for index in 0..REGISTRATION_COUNT {
        register_task(&authority, index);
    }
    let registration_elapsed = registered.elapsed();

    // Tier predicate: the landed registration face stays inside the tier.
    assert!(TASK_PROFILE_10K.admits_task_nodes(REGISTRATION_COUNT));

    // Re-registering an existing spec is idempotent via the same PK lookup.
    let replay = authority
        .register_task(TaskSpec {
            task_id: task_id(0),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("re-register task");
    assert_eq!(
        replay,
        nlos_task::TaskRegistrationDecision::Existing(task_id(0))
    );

    // Active working set: scattered tasks hold one outstanding permit each.
    // The unique partial index `commit_permits_single_active` makes a second
    // outstanding permit per task unrepresentable on disk, so the permit CAS
    // is O(active-per-task), never a scan of the permit table.
    let step = REGISTRATION_COUNT / ACTIVE_SAMPLE;
    let permits = Instant::now();
    for ordinal in 0..ACTIVE_SAMPLE {
        let index = ordinal * step;
        authority
            .register_attempt(attempt_spec(index))
            .expect("register attempt");
        let decision = authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                permit_request(index, 0x30),
            )
            .expect("request permit");
        let permit = issued_permit(decision);
        assert_eq!(permit.task_id, task_id(index));

        // A second attempt on the same task is fenced by the single-active
        // permit surface, not by scanning other tasks' permits.
        let loser = AttemptSpec {
            attempt_id: TaskAttemptId::from_bytes(id_bytes(0x03, index)),
            idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa1, index)),
            ..attempt_spec(index)
        };
        authority.register_attempt(loser).expect("register loser");
        let fenced = authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                PermitRequest {
                    attempt_id: TaskAttemptId::from_bytes(id_bytes(0x03, index)),
                    ..permit_request(index, 0x31)
                },
            )
            .expect("fenced permit request");
        match fenced {
            PermitDecision::Superseded { winner } => {
                assert_eq!(winner.permit_id, permit.permit_id);
            }
            other => panic!("expected Superseded, got {other:?}"),
        }
    }
    let permit_elapsed = permits.elapsed();

    // Point reads stay key-scoped at any sample position.
    for ordinal in [0, 1, ACTIVE_SAMPLE / 2, ACTIVE_SAMPLE - 1] {
        let record = authority
            .inspect_task(task_id(ordinal * step))
            .expect("inspect task");
        assert_eq!(record.task_id, task_id(ordinal * step));
    }

    assert!(
        TASK_PROFILE_10K.admits_active_working_set(ACTIVE_SAMPLE),
        "active sample must fit the tier"
    );
    let pressure = WorkingSetPressure::new(&TASK_PROFILE_10K, ACTIVE_SAMPLE);
    assert!(
        !pressure.needs_reclaim(),
        "small active sample must stay below soft reclaim threshold"
    );
    // Pathological-slowness guard only (complexity evidence is in the
    // ignored 10K probe; CI machines must never trip this).
    assert!(
        registration_elapsed < Duration::from_mins(1),
        "registration: {registration_elapsed:?}"
    );
    assert!(
        permit_elapsed < Duration::from_mins(1),
        "permit face: {permit_elapsed:?}"
    );
}
