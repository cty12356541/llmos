//! ROAD-B-004 B4-7 / W31-B：working-set 比例矩阵基准——当逻辑 population
//! 的 X% 处于活跃时，admission / pressure / reclaim advisory 的行为。
//!
//! 矩阵（诚实档位选择，全部为显式声明常量）：
//!
//! - 10K population × {1%, 10%, 50%} active → 工作集 100 / 1000 / 5000；
//! - 100K population × {1%, 10%, 50%} active → 工作集 1000 / 10000 / 50000；
//! - 默认套件另有 500 population 三比例 smoke（同一管线，小规模全断言）。
//!
//! 口径（ADR-0016 决定 4 维度纪律，沿 W31-A §10.7 词汇规则）：
//!
//! 1. **working-set 维度 = 未决 `CommitPermit` 计数**——W18/W19 落地的
//!    `request_commit_permit` admission 前缀与 `inspect_working_set_pressure`
//!    读面测的就是这个 durable 事实；不与 plan 侧物化窗口计数混写。
//! 2. **逻辑 population 由每 cell profile 的注册维度承载**
//!    （`max_task_registrations == population`，两声明维度同量级）；声明
//!    `TaskNode`（`plan_nodes`）维度的 10K/100K 数字归 W31-D
//!    （`nlos-plan` `tasknode_scale_probe.rs`），两口径不混写。
//! 3. **超过已发布档 ~5% 工作集姿态的 cell（10K@10%/50%、100K@10%/50%）是
//!    对 admission 机制的刻意超比例探测**：每 cell 用显式 per-cell
//!    `ScaleProfile`（`max_active_working_set == population × ratio`）；
//!    已发布 `TASK_PROFILE_10K`/`TASK_PROFILE_100K` 本身在该占用上必须
//!    fail-closed（探针内以 `enforce_working_set_admission` 锚定断言），
//!    即本矩阵不声称已发布档支持 >5% 活跃比例。
//! 4. **W31-A consult 面同测**：`answer_plan_materialization`（Task 侧
//!    只读消费路径）在空载 / 软阈值下 / 工作集饱和 / task-node 维饱和
//!    四个位置的 verdict 逐位断言。
//!
//! 确定性纪律（比 W31-D 更严，按本车道验收门「no assertion on
//! wall-clock」）：**全部计时数字只记录、零断言**；本探针的断言只有
//! 每 ratio 位置的精确事实——issuance 在 cap 内逐笔 `Issued`、advisory
//! 恰在 `(threshold, cap]` 投影带内携带且字段逐位相等、
//! execution/outcome 链在带内首笔与 cap 笔形状精确、pressure 快照在
//! {0, threshold, threshold+1, cap} 计数点与独立推导的期望逐位相等、
//! cap+1 issuance 以 typed 三元组 fail-closed、饱和后 consult 两维
//! verdict 精确、cap 上幂等 replay 绕过全部前缀（advisory/execution/
//! outcome 皆 `None`）。
//!
//! 探针为显式 `#[ignore]`（会物化真实 authority 数据库）；复现命令：
//!
//! ```sh
//! cargo test -p nlos-task --test working_set_ratio_probe -- --ignored --nocapture
//! ```
//!
//! 实测数字原样誊录进 `docs/evidence/stage-b/b-task-scale-001.md` §13。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_task::{
    AttemptSpec, Authorities, DEFAULT_RECLAIM_THRESHOLD_RATIO, MaterializationAdmissionFacts,
    PermitDecision, PermitRequest, ReclaimPhase, ScaleProfile, SnapshotBundle, SqliteTaskAuthority,
    TASK_PROFILE_10K, TASK_PROFILE_100K, TaskSpec, TaskStoreError, WorkingSetPressureSnapshot,
    WorkingSetReclaimAdvisory, WorkingSetReclaimExecution, WorkingSetReclaimExecutionRequest,
    WorkingSetReclaimOutcome, empty_effect_history_root, enforce_working_set_admission,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

// -- 矩阵 cell 声明（诚实档位：显式常量，非运行期计算） --------------------

static CELL_SMOKE_1PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-smoke-1pct",
    max_task_nodes: 500,
    max_task_registrations: 500,
    max_active_working_set: 5,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_SMOKE_10PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-smoke-10pct",
    max_task_nodes: 500,
    max_task_registrations: 500,
    max_active_working_set: 50,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_SMOKE_50PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-smoke-50pct",
    max_task_nodes: 500,
    max_task_registrations: 500,
    max_active_working_set: 250,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_10K_1PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-10k-1pct",
    max_task_nodes: 10_000,
    max_task_registrations: 10_000,
    max_active_working_set: 100,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_10K_10PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-10k-10pct",
    max_task_nodes: 10_000,
    max_task_registrations: 10_000,
    max_active_working_set: 1_000,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_10K_50PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-10k-50pct",
    max_task_nodes: 10_000,
    max_task_registrations: 10_000,
    max_active_working_set: 5_000,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_100K_1PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-100k-1pct",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 1_000,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_100K_10PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-100k-10pct",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 10_000,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

static CELL_100K_50PCT: ScaleProfile = ScaleProfile {
    profile_id: "task-ratio-100k-50pct",
    max_task_nodes: 100_000,
    max_task_registrations: 100_000,
    max_active_working_set: 50_000,
    reclaim_threshold_ratio: Some(DEFAULT_RECLAIM_THRESHOLD_RATIO),
};

/// One matrix cell: a per-cell profile plus the population/ratio pair it
/// declares. `population × ratio_percent / 100` must equal
/// `profile.max_active_working_set` exactly (pinned by the pure test).
#[derive(Clone, Copy)]
struct MatrixCell {
    profile: &'static ScaleProfile,
    population: u64,
    ratio_percent: u64,
}

const SMOKE_CELLS: [MatrixCell; 3] = [
    MatrixCell {
        profile: &CELL_SMOKE_1PCT,
        population: 500,
        ratio_percent: 1,
    },
    MatrixCell {
        profile: &CELL_SMOKE_10PCT,
        population: 500,
        ratio_percent: 10,
    },
    MatrixCell {
        profile: &CELL_SMOKE_50PCT,
        population: 500,
        ratio_percent: 50,
    },
];

const TEN_K_CELLS: [MatrixCell; 3] = [
    MatrixCell {
        profile: &CELL_10K_1PCT,
        population: 10_000,
        ratio_percent: 1,
    },
    MatrixCell {
        profile: &CELL_10K_10PCT,
        population: 10_000,
        ratio_percent: 10,
    },
    MatrixCell {
        profile: &CELL_10K_50PCT,
        population: 10_000,
        ratio_percent: 50,
    },
];

const HUNDRED_K_CELLS: [MatrixCell; 2] = [
    MatrixCell {
        profile: &CELL_100K_1PCT,
        population: 100_000,
        ratio_percent: 1,
    },
    MatrixCell {
        profile: &CELL_100K_10PCT,
        population: 100_000,
        ratio_percent: 10,
    },
];

/// The published tier a cell's population belongs to (honest anchor); smoke
/// cells have none.
fn published_tier_for(population: u64) -> Option<&'static ScaleProfile> {
    if population == TASK_PROFILE_10K.max_task_registrations {
        Some(&TASK_PROFILE_10K)
    } else if population == TASK_PROFILE_100K.max_task_registrations {
        Some(&TASK_PROFILE_100K)
    } else {
        None
    }
}

// -- 期望构造（独立于被测 API 推导，逐位比较） ------------------------------

fn advisory_at(profile: &ScaleProfile, projected_active_count: u64) -> WorkingSetReclaimAdvisory {
    WorkingSetReclaimAdvisory {
        profile_id: profile.profile_id,
        projected_active_count,
        reclaim_threshold_count: profile.reclaim_threshold_count(),
        reclaim_threshold_ratio: profile.effective_reclaim_threshold_ratio(),
        max_active_working_set: profile.max_active_working_set,
    }
}

/// Advisory expectation for a net-new issuance projecting
/// `projected_active_count` occupancy: `Some` exactly inside the
/// `(threshold, cap]` band, `None` below the soft threshold or over the cap.
fn expected_advisory_at(
    profile: &ScaleProfile,
    projected_active_count: u64,
) -> Option<WorkingSetReclaimAdvisory> {
    let inside_band = profile.reclaim_threshold_count() < projected_active_count
        && projected_active_count <= profile.max_active_working_set;
    inside_band.then(|| advisory_at(profile, projected_active_count))
}

/// Read-side pressure expectation at `active_count` occupancy, derived
/// independently from the profile constants.
fn expected_snapshot(profile: &ScaleProfile, active_count: u64) -> WorkingSetPressureSnapshot {
    let needs_reclaim = active_count > profile.reclaim_threshold_count();
    let admits = profile.admits_active_working_set(active_count);
    WorkingSetPressureSnapshot {
        profile_id: profile.profile_id,
        active_count,
        reclaim_threshold_count: profile.reclaim_threshold_count(),
        needs_reclaim,
        admits,
        reclaim_advisory: (needs_reclaim && admits).then(|| advisory_at(profile, active_count)),
    }
}

// -- 探针 harness（镜像 scale_profile_probe / tasknode_scale_probe 姿态） ----

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-ws-ratio-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open_with_profile(&self, profile: &'static ScaleProfile) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open_with_scale_profile(&self.path, profile)
            .expect("open task authority with cell profile")
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
    let decision = authority
        .register_task(TaskSpec {
            task_id: task_id(index),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
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

/// Competing second attempt on an already-permitted task — the one-past-cap
/// deny probe. The admission prefix consults the saturated working-set count
/// before `compete_for_permit` ever runs, and the cell profiles deliberately
/// leave no spare registration slot (both declared dimensions carry the
/// population exactly), so the deny face is probed through a net-new request
/// on an occupied task rather than a fresh registration.
fn deny_attempt_spec(index: u64) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(index),
        attempt_id: TaskAttemptId::from_bytes(id_bytes(0x12, index)),
        attempt_generation: Generation::INITIAL,
        snapshot: snapshot(index),
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(0xc1, index)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa2, index)),
        registered_at_ms: 2_000,
    }
}

fn deny_permit_request(index: u64) -> PermitRequest {
    PermitRequest {
        task_id: task_id(index),
        attempt_id: TaskAttemptId::from_bytes(id_bytes(0x12, index)),
        attempt_generation: Generation::INITIAL,
        write_set_root: [0x41; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xb1, index)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

/// Evenly scattered active-slot position across the population ID space.
fn sample_position(ordinal: u64, population: u64, active: u64) -> u64 {
    ordinal * population / active
}

fn percentile(sorted: &[Duration], per_myriad: u32) -> Duration {
    let denominator = u64::try_from(sorted.len()).expect("sample fits u64");
    let position = (denominator * u64::from(per_myriad))
        .div_ceil(10_000)
        .clamp(1, denominator);
    sorted[usize::try_from(position - 1).expect("position fits usize")]
}

fn assert_issued(decision: PermitDecision, index: u64) {
    match decision {
        PermitDecision::Issued(record) => assert_eq!(record.task_id, task_id(index)),
        other => panic!("expected Issued inside the cap, got {other:?}"),
    }
}

/// The full W31-B ratio-matrix cell pipeline. `print` toggles the profile
/// line (the default-suite smoke stays quiet); every assertion is identical
/// in every cell, and **no assertion reads the clock**.
#[allow(clippy::too_many_lines)]
fn run_working_set_ratio_matrix_cell(cell: &MatrixCell, print: bool) {
    let profile = cell.profile;
    let population = cell.population;
    let ratio_percent = cell.ratio_percent;
    let active = profile.max_active_working_set;
    let threshold = profile.reclaim_threshold_count();

    // Cell preconditions (the pure test pins the same arithmetic; the probe
    // re-checks so a mis-declared cell fails closed before materializing).
    assert_eq!(profile.max_task_registrations, population);
    assert_eq!(profile.max_task_nodes, population);
    assert_eq!(active * 100, population * ratio_percent);
    assert!(threshold > 0 && threshold < active && active < population);
    assert_eq!(profile.effective_reclaim_threshold_ratio(), 90);

    // Honest anchor against the published tiers: a cell beyond the ~5%
    // working-set posture is a deliberate over-ratio probe of the admission
    // mechanism — the published tier itself must fail closed at that
    // occupancy, deterministically.
    let published_within = published_tier_for(population).is_none_or(|published| {
        let within = active <= published.max_active_working_set;
        assert_eq!(
            enforce_working_set_admission(published, active).is_ok(),
            within,
            "published-tier anchor for {}",
            profile.profile_id
        );
        within
    });

    let rss_before = sample_rss_bytes();
    let database = TestDatabase::new(profile.profile_id);
    let authority = database.open_with_profile(profile);

    // -- Logical population (registration dimension, fsynced per row). -----
    let register_started = Instant::now();
    for index in 0..population {
        register_task(&authority, index);
    }
    let register_elapsed = register_started.elapsed();

    // -- Empty occupancy: pressure readback + W31-A consult face. -----------
    let inspect_empty_started = Instant::now();
    assert_eq!(
        authority
            .inspect_working_set_pressure()
            .expect("inspect at zero occupancy"),
        expected_snapshot(profile, 0)
    );
    let inspect_empty_elapsed = inspect_empty_started.elapsed();

    let consult_empty_started = Instant::now();
    let consult_empty = authority
        .answer_plan_materialization(0)
        .expect("consult at zero occupancy");
    let consult_empty_elapsed = consult_empty_started.elapsed();
    assert_eq!(
        consult_empty,
        MaterializationAdmissionFacts {
            profile_id: profile.profile_id,
            projected_task_nodes: 1,
            projected_active_working_set: 1,
        }
    );

    // -- Attempts for every active slot, scattered over the ID space. ------
    let attempts_started = Instant::now();
    for ordinal in 0..active {
        authority
            .register_attempt(attempt_spec(sample_position(ordinal, population, active)))
            .expect("register attempt");
    }
    let attempts_elapsed = attempts_started.elapsed();

    // -- Fill loop: exact admission/advisory facts at EVERY ratio position.
    let fill_started = Instant::now();
    let mut fill_latencies =
        Vec::with_capacity(usize::try_from(active).expect("active fits usize"));
    let mut inspect_at_threshold = None;
    let mut inspect_at_threshold_plus_one = None;
    let mut inspect_at_cap = None;
    let mut consult_below_cap = None;
    let mut last_execution = None;
    for ordinal in 0..active {
        let projected = ordinal + 1;
        let started = Instant::now();
        let decision = authority
            .request_commit_permit_decision_with_authorities_struct(
                Authorities::default(),
                permit_request(sample_position(ordinal, population, active), 0x40),
            )
            .expect("issuance inside the cap");
        fill_latencies.push(started.elapsed());
        assert_issued(
            decision.permit,
            sample_position(ordinal, population, active),
        );

        // Advisory expectation: Some exactly inside (threshold, cap].
        let expected_advisory = expected_advisory_at(profile, projected);
        assert_eq!(
            decision.reclaim_advisory, expected_advisory,
            "advisory at projected {projected}"
        );
        if (projected == threshold + 1 || projected == active)
            && let Some(advisory) = expected_advisory
        {
            assert_eq!(
                decision.reclaim_execution,
                Some(WorkingSetReclaimExecution {
                    execution_sequence: 0,
                    phase: ReclaimPhase::RebuildableCache,
                    advisory,
                })
            );
            assert_eq!(
                decision.reclaim_outcome,
                Some(WorkingSetReclaimOutcome {
                    execution_sequence: 0,
                    phase: ReclaimPhase::RebuildableCache,
                    evicted_units: projected - threshold,
                    advisory,
                })
            );
        }

        // Pressure checkpoints at the semantic counts (exact snapshots).
        if projected == threshold {
            let started = Instant::now();
            assert_eq!(
                authority
                    .inspect_working_set_pressure()
                    .expect("inspect at soft threshold"),
                expected_snapshot(profile, projected)
            );
            inspect_at_threshold = Some(started.elapsed());
            let started = Instant::now();
            let facts = authority
                .answer_plan_materialization(0)
                .expect("consult below cap at soft threshold");
            consult_below_cap = Some(started.elapsed());
            assert_eq!(
                facts,
                MaterializationAdmissionFacts {
                    profile_id: profile.profile_id,
                    projected_task_nodes: 1,
                    projected_active_working_set: threshold + 1,
                }
            );
        }
        if projected == threshold + 1 && projected != active {
            let started = Instant::now();
            assert_eq!(
                authority
                    .inspect_working_set_pressure()
                    .expect("inspect above soft threshold"),
                expected_snapshot(profile, projected)
            );
            inspect_at_threshold_plus_one = Some(started.elapsed());
        }
        if projected == active {
            last_execution = decision.reclaim_execution;
            let started = Instant::now();
            assert_eq!(
                authority
                    .inspect_working_set_pressure()
                    .expect("inspect at cap"),
                expected_snapshot(profile, projected)
            );
            inspect_at_cap = Some(started.elapsed());
        }
    }
    let fill_elapsed = fill_started.elapsed();
    fill_latencies.sort_unstable();

    // -- Saturated consults: working-set dimension denies with the exact
    //    projected triple; the declared-node dimension is consulted first.
    let consult_ws_started = Instant::now();
    let ws_denied = authority
        .answer_plan_materialization(active - 1)
        .expect_err("consult at saturated working set");
    let consult_ws_elapsed = consult_ws_started.elapsed();
    assert!(matches!(
        ws_denied,
        TaskStoreError::WorkingSetAdmissionDenied {
            profile_id,
            active_count,
            max_active_working_set,
        } if profile_id == cell.profile.profile_id
            && active_count == active + 1
            && max_active_working_set == active
    ));

    let consult_tn_started = Instant::now();
    let tn_denied = authority
        .answer_plan_materialization(profile.max_task_nodes)
        .expect_err("consult with saturated declared-node dimension");
    let consult_tn_elapsed = consult_tn_started.elapsed();
    assert!(matches!(
        tn_denied,
        TaskStoreError::TaskNodeAdmissionDenied {
            profile_id,
            task_count,
            max_task_nodes,
        } if profile_id == cell.profile.profile_id
            && task_count == profile.max_task_nodes + 1
            && max_task_nodes == profile.max_task_nodes
    ));

    // -- One-past-cap issuance fails closed with the exact typed triple. ---
    let deny_index = sample_position(0, population, active);
    authority
        .register_attempt(deny_attempt_spec(deny_index))
        .expect("register deny-probe attempt");
    let deny_started = Instant::now();
    let denied = authority
        .request_commit_permit_decision_with_authorities_struct(
            Authorities::default(),
            deny_permit_request(deny_index),
        )
        .expect_err("issuance one past the cap must fail closed");
    let deny_elapsed = deny_started.elapsed();
    assert!(matches!(
        denied,
        TaskStoreError::WorkingSetAdmissionDenied {
            profile_id,
            active_count,
            max_active_working_set,
        } if profile_id == cell.profile.profile_id
            && active_count == active + 1
            && max_active_working_set == active
    ));

    // Denial adds no occupancy: the snapshot is unchanged.
    assert_eq!(
        authority
            .inspect_working_set_pressure()
            .expect("inspect after denial"),
        expected_snapshot(profile, active)
    );

    // -- Idempotent replay at the cap bypasses the gate and all prefixes. --
    let replay_started = Instant::now();
    let replay = authority
        .request_commit_permit_decision_with_authorities_struct(
            Authorities::default(),
            permit_request(sample_position(0, population, active), 0x40),
        )
        .expect("replay at cap");
    let replay_elapsed = replay_started.elapsed();
    match replay.permit {
        PermitDecision::Issued(record) | PermitDecision::Replayed(record) => {
            assert_eq!(
                record.task_id,
                task_id(sample_position(0, population, active))
            );
        }
        other => panic!("expected Issued or Replayed, got {other:?}"),
    }
    assert!(replay.reclaim_advisory.is_none());
    assert!(replay.reclaim_execution.is_none());
    assert!(replay.reclaim_outcome.is_none());

    // -- Reclaim then re-enter: occupancy must not only monotonically fill
    //    (W36-P8; W31-G §8.2.7). Drive the cap issuance's warrant, then
    //    issue on unused population members until occupancy rises again.
    let execution = last_execution.expect("cap issuance surfaces a reclaim warrant");
    let reclaim_started = Instant::now();
    let reclaim_report = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution,
            executed_at_ms: 8_000,
        })
        .expect("drive reclaim after cap fill");
    let reclaim_elapsed = reclaim_started.elapsed();
    assert!(
        reclaim_report.post_active_count < reclaim_report.pre_active_count,
        "reclaim must drop occupancy"
    );
    assert_eq!(reclaim_report.pre_active_count, active);
    assert_eq!(reclaim_report.post_active_count, threshold);

    let used: HashSet<u64> = (0..active)
        .map(|ordinal| sample_position(ordinal, population, active))
        .collect();
    let reenter = reclaim_report.pre_active_count - reclaim_report.post_active_count;
    let unused: Vec<u64> = (0..population)
        .filter(|index| !used.contains(index))
        .take(usize::try_from(reenter).expect("reenter fits usize"))
        .collect();
    assert_eq!(unused.len() as u64, reenter);

    let reenter_started = Instant::now();
    for index in unused {
        authority
            .register_attempt(attempt_spec(index))
            .expect("register reentry attempt");
        let decision = authority
            .request_commit_permit_decision_with_authorities_struct(
                Authorities::default(),
                permit_request(index, 0x50),
            )
            .expect("reentry issuance");
        assert_issued(decision.permit, index);
    }
    let reenter_elapsed = reenter_started.elapsed();
    let after_reenter = authority
        .inspect_working_set_pressure()
        .expect("inspect after reentry");
    assert!(
        after_reenter.active_count > reclaim_report.post_active_count,
        "reentry must raise occupancy after reclaim"
    );
    assert_eq!(after_reenter.active_count, active);

    let rss_after = sample_rss_bytes();
    drop(authority);
    let database_bytes = file_size(&database.path);

    if print {
        let profile_id = profile.profile_id;
        eprintln!(
            "W31-B ratio matrix cell {profile_id} (single platform): \
             population={population} ratio={ratio_percent}% active={active} \
             threshold={threshold} published_within={published_within} \
             register_total={register_elapsed:?} attempts_total={attempts_elapsed:?} \
             fill_total={fill_elapsed:?} fill_p50={:?} fill_p95={:?} fill_max={:?} \
             inspect_empty={inspect_empty_elapsed:?} \
             inspect_at_threshold={inspect_at_threshold:?} \
             inspect_at_threshold_plus_one={inspect_at_threshold_plus_one:?} \
             inspect_at_cap={inspect_at_cap:?} \
             consult_empty={consult_empty_elapsed:?} \
             consult_below_cap={consult_below_cap:?} \
             consult_ws_saturated={consult_ws_elapsed:?} \
             consult_tn_saturated={consult_tn_elapsed:?} \
             deny_latency={deny_elapsed:?} replay_latency={replay_elapsed:?} \
             reclaim_total={reclaim_elapsed:?} reenter_total={reenter_elapsed:?} \
             reclaim_pre={:?} reclaim_post={:?} \
             database_bytes={database_bytes} \
             rss_before={rss_before:?} rss_after={rss_after:?}",
            fill_latencies[fill_latencies.len() / 2],
            percentile(&fill_latencies, 9_500),
            fill_latencies[fill_latencies.len() - 1],
            reclaim_report.pre_active_count,
            reclaim_report.post_active_count,
        );
    }
}

#[test]
fn matrix_cells_declare_exact_ratios_and_published_tier_anchors() {
    let mut cells = Vec::new();
    cells.extend(SMOKE_CELLS);
    cells.extend(TEN_K_CELLS);
    cells.extend(HUNDRED_K_CELLS);
    cells.push(MatrixCell {
        profile: &CELL_100K_50PCT,
        population: 100_000,
        ratio_percent: 50,
    });
    assert_eq!(cells.len(), 9);

    for cell in cells {
        let profile = cell.profile;
        let active = profile.max_active_working_set;
        let threshold = profile.reclaim_threshold_count();
        // Exact integer ratio: active == population x ratio%.
        assert_eq!(
            active * 100,
            cell.population * cell.ratio_percent,
            "{}",
            profile.profile_id
        );
        // Both declared dimensions carry the logical population.
        assert_eq!(profile.max_task_registrations, cell.population);
        assert_eq!(profile.max_task_nodes, cell.population);
        // The advisory band (threshold, cap] is non-empty and a deny slot
        // exists beyond the cap.
        assert!(threshold > 0 && threshold < active && active < cell.population);
        assert_eq!(profile.effective_reclaim_threshold_ratio(), 90);
        // Published-tier anchor: within-posture cells would still be
        // admitted by the published tier; beyond-posture cells are exactly
        // the ones the published tier fails closed on.
        if let Some(published) = published_tier_for(cell.population) {
            let within = active <= published.max_active_working_set;
            assert_eq!(
                enforce_working_set_admission(published, active).is_ok(),
                within,
                "published-tier anchor for {}",
                profile.profile_id
            );
        }
    }

    // The published tiers pin the ~5% working-set posture the matrix ladder
    // is anchored against (5% admits, 6% denies on both tiers).
    assert!(TASK_PROFILE_10K.admits_active_working_set(500));
    assert!(!TASK_PROFILE_10K.admits_active_working_set(600));
    assert!(TASK_PROFILE_100K.admits_active_working_set(5_000));
    assert!(!TASK_PROFILE_100K.admits_active_working_set(6_000));
}

/// Default-suite smoke: the full matrix-cell pipeline at a small population
/// keeps every helper and exact assertion exercised without materializing
/// the 10K/100K databases.
#[test]
fn small_tier_ratio_matrix_smoke() {
    for cell in SMOKE_CELLS {
        run_working_set_ratio_matrix_cell(&cell, false);
    }
}

#[test]
#[ignore = "explicit ROAD-B-004 B4-7 W31-B 10K working-set ratio matrix probe"]
fn ten_thousand_population_ratio_matrix_admits_correctly() {
    for cell in TEN_K_CELLS {
        run_working_set_ratio_matrix_cell(&cell, true);
    }
}

#[test]
#[ignore = "explicit ROAD-B-004 B4-7 W31-B 100K working-set ratio matrix probe"]
fn one_hundred_thousand_population_ratio_matrix_admits_correctly() {
    for cell in HUNDRED_K_CELLS {
        run_working_set_ratio_matrix_cell(&cell, true);
    }
}

#[test]
#[ignore = "explicit W36-P8 100K@50% working-set ratio cell + reclaim-reentry (W31-G §8.2.7)"]
fn one_hundred_thousand_population_fifty_percent_ratio_matrix_and_reentry() {
    run_working_set_ratio_matrix_cell(
        &MatrixCell {
            profile: &CELL_100K_50PCT,
            population: 100_000,
            ratio_percent: 50,
        },
        true,
    );
}
