//! ROAD-B-004 B4-9 / 议题 35 §6 G2+G5 后半（W31-D）：10K/100K 逻辑
//! `TaskNode` 规模探针，跑在已落地的 durable plan authority 上。
//!
//! 这些探针经已落地的 `apply_plan_revision` API 声明上万个**逻辑
//! `TaskNode`**——真实的、有界的 `plan_nodes` durable metadata 行
//! （`[SCALE-LOGICAL-001]` 姿态）。按 house 诚实规则：**waiting Fiber
//! 不能替代 `TaskNode` benchmark**；也**不再用 Task 注册近似承载**
//! `max_task_nodes`（ADR-0016 决定 4 已把该维度正规化为 `plan_nodes`
//! 持久计数，Task 注册保留为独立第二维度，口径切换见
//! `b-task-scale-001.md` §12——本探针测的就是正规化后的逻辑计数本身）。
//!
//! 探针为显式 `#[ignore]`（会物化真实 authority 数据库）；复现命令：
//!
//! ```sh
//! cargo test -p nlos-plan --test tasknode_scale_probe -- --ignored --nocapture
//! ```
//!
//! 测量维度（单平台，原样誊录进
//! `docs/evidence/stage-b/b-plan-001-declaration-surface.md` §9）：
//!
//! 1. **声明吞吐**：一个 revision 携带全量节点集（混合依赖形状：约 9%
//!    独立 root、约 90% 链式 dependent、约 1% 挂在 0 号 hub 上的叶），
//!    单事务一次 fsync 应用；
//! 2. **metadata 足迹**：每逻辑节点 durable 字节数（硬上界断言）与
//!    声明 + resolver 活动前后的进程 RSS 增量（硬上界断言——G2
//!    「未物化节点不预占执行资源」的可测代理）；
//! 3. **惰性读面**：100 节点基线库 vs 10K/100K 库上 `inspect_node` 与
//!    `inspect_node_residency` 的 p95 对比（小常数倍 + 绝对上限，
//!    镜像 `b-task-scale-001` 探针姿态）；
//! 4. **resolver + 分级读回规模面**：`resolve_plan`、
//!    `inspect_resolved_nodes` 与生命周期/residency 活动样本
//!    （`DECLARED→ELIGIBLE` 样本；`METADATA_ONLY→COLD→WARM→HOT` 上行
//!    再 `HOT→WARM→COLD` evict 下行）——如实计时记录，并断言
//!    metadata 事实跨 evict 逐位保留。
//!
//! 确定性纪律：吞吐数字只记录、不断言；仅有的断言是硬上界
//! （每节点 durable 字节、RSS 上限、活动字节增量）与惰性对比面。

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_plan::{
    ApplyPlanRevisionRequest, NodeResidencyTier, NodeTransitionRequest, PlanNodeDeclaration,
    PlanNodeKind, PlanNodeState, PlanResolutionHandle, PlanRevisionSelector,
    ResidencyTransitionRequest, ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, TaskNodeId, TaskPlanId};

const BASELINE_COUNT: u64 = 100;
const LAZY_SAMPLE: usize = 64;
const EVICT_SAMPLE: usize = 8;
/// G2 硬上界：每个逻辑 `TaskNode` 的 durable metadata 字节数（含
/// `plan_nodes` 行、per-revision 形状行、依赖边行、解析回执内联 blob
/// 与页开销的分摊）。任何随节点数的超线性 metadata 增长都会击穿它。
const MAX_DURABLE_BYTES_PER_NODE: u64 = 4096;
/// 声明之后的解析/迁移活动（解析回执 + 少量凭证）允许追加的持久字节，
/// 按节点摊销：回执内联 `resolved_order`(16B/节点) +
/// `resolved_edges`(32B/边) 是主要的合法增量。
const MAX_ACTIVITY_BYTES_PER_NODE: u64 = 256;
const ACTIVITY_BYTES_SLACK: u64 = 64 * 1024;
/// 惰性断言的相对面：规模库上的点读 p95 不得超过基线库 p95 的这个小
/// 常数倍（全表扫描式退化在这里会高出 ~100x）。
const LAZY_RATIO: u32 = 16;
/// 惰性断言的绝对面：捕获病态回归的宽松单次点读上限。
const ABSOLUTE_LAZY_CEILING: Duration = Duration::from_millis(100);

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-plan-tasknode-scale-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> SqlitePlanAuthority {
        SqlitePlanAuthority::open(&self.path).expect("open plan authority")
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

/// Clean-close 之后的 durable 足迹：主库 + 残留 WAL（干净关闭时
/// checkpoint 会把 WAL 折回主库，这里仍求和以保持诚实）。
fn durable_bytes(database: &TestDatabase) -> u64 {
    file_size(&database.path) + file_size(&suffix_path(&database.path, "-wal"))
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

/// Deterministic 16-byte identity: `tag` (u64 BE) ‖ `index` (u64 BE).
/// Node keys, idempotency keys, and per-step keys all use disjoint tags.
fn bytes16(tag: u64, index: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&tag.to_be_bytes());
    bytes[8..].copy_from_slice(&index.to_be_bytes());
    bytes
}

/// The declaration index encoded in a `node_key` (reverse of [`bytes16`]).
fn index_of(node_key: [u8; 16]) -> u64 {
    u64::from_be_bytes(node_key[8..].try_into().expect("node key tail"))
}

fn node_key(index: u64) -> [u8; 16] {
    bytes16(0x4e_6f_64_65_4b_65_79, index)
}

fn digest32(seed: u64) -> [u8; 32] {
    let mut bytes = [0_u8; 32];
    for (position, byte) in seed.to_be_bytes().iter().enumerate() {
        bytes[position] = *byte;
        bytes[position + 8] = *byte;
        bytes[position + 16] = *byte;
        bytes[position + 24] = *byte;
    }
    bytes
}

/// One node's mixed-shape declaration. The dependency mix (per G5 的混合
/// 依赖形状要求，确定性构造)：
///
/// - `index % 10 != 0`：链式成员，依赖 `index - 1`（「pending」面——
///   解析前被前驱阻塞的 dependent，约 90%）；
/// - `index > 0 && index % 100 == 0`：hub 叶，额外依赖 0 号节点
///   （周期性汇聚依赖，约 1%，给 resolver 的 dependents 面加压）；
/// - 其余（含 0 号）：独立 root（无依赖、解析后即可 eligibility 的
///   「resolved」面，约 9%）。
///
/// 所有边都严格指向更小的 index，构造上无环。
fn declaration(index: u64) -> PlanNodeDeclaration {
    let mut dependency_keys = Vec::new();
    if !index.is_multiple_of(10) {
        dependency_keys.push(node_key(index - 1));
    }
    if index > 0 && index.is_multiple_of(100) {
        dependency_keys.push(node_key(0));
    }
    PlanNodeDeclaration {
        node_key: node_key(index),
        kind: if index.is_multiple_of(2) {
            PlanNodeKind::AgentRole
        } else {
            PlanNodeKind::Executable
        },
        binding_digest: digest32(index ^ 0x1111_1111_1111_1111),
        dependency_keys,
        input_selectors_digest: digest32(index ^ 0x2222_2222_2222_2222),
        output_contract_digest: digest32(index ^ 0x3333_3333_3333_3333),
        policy_digest: digest32(index ^ 0x4444_4444_4444_4444),
        resource_ceiling_digest: digest32(index ^ 0x5555_5555_5555_5555),
        conditions: None,
    }
}

fn declaration_set(count: u64) -> Vec<PlanNodeDeclaration> {
    (0..count).map(declaration).collect()
}

fn expected_edge_count(count: u64) -> u64 {
    let chain_members = count - count.div_ceil(10);
    // Hub leaves are the nonzero multiples of 100 below `count`.
    let hub_leaves = (count - 1) / 100;
    chain_members + hub_leaves
}

/// Root（零依赖）声明 index：0 与「10 的倍数但非 100 的倍数」。
fn root_indices(count: u64, limit: usize) -> Vec<u64> {
    let mut roots = Vec::new();
    let mut index = 0_u64;
    while index < count && roots.len() < limit {
        if index == 0 || !index.is_multiple_of(100) {
            roots.push(index);
        }
        index += 10;
    }
    roots
}

/// Evenly scattered sample positions across the declared index space.
fn sample_indices(count: u64, sample: usize) -> Vec<u64> {
    let sample_u64 = u64::try_from(sample).expect("sample fits u64");
    (0..sample_u64)
        .map(|position| position * count / sample_u64)
        .collect()
}

fn percentile(sorted: &[Duration], per_myriad: u32) -> Duration {
    let denominator = u64::try_from(sorted.len()).expect("sample fits u64");
    let position = (denominator * u64::from(per_myriad))
        .div_ceil(10_000)
        .clamp(1, denominator);
    sorted[usize::try_from(position - 1).expect("position fits usize")]
}

fn apply_revision(
    authority: &SqlitePlanAuthority,
    node_count: u64,
    key: IdempotencyKey,
) -> (TaskPlanId, Duration) {
    let request = ApplyPlanRevisionRequest {
        plan_id: None,
        nodes: declaration_set(node_count),
        idempotency_key: key,
        applied_at_ms: 1_000,
    };
    let started = Instant::now();
    let receipt = authority
        .apply_plan_revision_ungated(request)
        .expect("apply plan revision")
        .receipt();
    let elapsed = started.elapsed();
    assert_eq!(receipt.revision, 1);
    assert_eq!(receipt.declared_node_count, node_count);
    (receipt.plan_id, elapsed)
}

fn resolve_current(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    key: IdempotencyKey,
) -> (PlanResolutionHandle, Duration) {
    let request = ResolvePlanRequest {
        selector: PlanRevisionSelector::Current(plan_id),
        idempotency_key: key,
        resolved_at_ms: 2_000,
    };
    let started = Instant::now();
    let handle = authority
        .resolve_plan(request)
        .expect("resolve plan")
        .handle();
    let elapsed = started.elapsed();
    assert_eq!(handle.revision, 1);
    (handle, elapsed)
}

/// Key-scoped `inspect_node` point reads over scattered nodes; each read
/// is timed individually and the results are sorted for percentiles.
fn inspect_node_latencies(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_ids: &[TaskNodeId],
) -> Vec<Duration> {
    node_ids
        .iter()
        .map(|node_id| {
            let started = Instant::now();
            let record = authority
                .inspect_node(plan_id, *node_id)
                .expect("inspect node")
                .expect("declared node reads back");
            assert_eq!(record.node_id, *node_id);
            started.elapsed()
        })
        .collect()
}

/// Key-scoped graded residency readback (`inspect_node_residency`) over
/// the same scattered nodes.
fn inspect_residency_latencies(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_ids: &[TaskNodeId],
) -> Vec<Duration> {
    node_ids
        .iter()
        .map(|node_id| {
            let started = Instant::now();
            let view = authority
                .inspect_node_residency(plan_id, *node_id)
                .expect("inspect node residency")
                .expect("declared node reads back");
            assert_eq!(view.node_id, *node_id);
            assert_eq!(view.tier, NodeResidencyTier::MetadataOnly);
            assert_eq!(view.transition_count, 0);
            started.elapsed()
        })
        .collect()
}

/// 生命周期活动样本：把 root 节点从 `DECLARED` 推进到 `ELIGIBLE`
/// （无依赖、解析后即可 eligibility 的「resolved」面）；其余节点保持
/// `DECLARED`（「pending」面）。每次迁移独立事务计时。
fn record_eligible_transitions(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    nodes: &[(u64, TaskNodeId)],
) -> (Duration, Duration) {
    let mut total = Duration::ZERO;
    let mut worst = Duration::ZERO;
    for (index, node_id) in nodes {
        let request = NodeTransitionRequest {
            plan_id,
            node_id: *node_id,
            from_state: PlanNodeState::Declared,
            to_state: PlanNodeState::Eligible,
            expected_declared_revision: 1,
            idempotency_key: IdempotencyKey::from_bytes(bytes16(0xE1, *index)),
            transitioned_at_ms: 3_000,
        };
        let started = Instant::now();
        authority
            .record_node_transition(request)
            .expect("record eligible transition");
        let elapsed = started.elapsed();
        total += elapsed;
        worst = worst.max(elapsed);
    }
    (total, worst)
}

/// 单个节点的 residency 全链走位：`METADATA_ONLY→COLD→WARM→HOT` 上行
/// （rehydrate 姿态），随后 `HOT→WARM→COLD` evict 下行；每步一张独立
/// 凭证、独立事务计时。
fn walk_residency_chain(
    authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    index: u64,
    node_id: TaskNodeId,
) -> Vec<Duration> {
    const STEPS: [(NodeResidencyTier, NodeResidencyTier, u64); 5] = [
        (
            NodeResidencyTier::MetadataOnly,
            NodeResidencyTier::Cold,
            0xF1,
        ),
        (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0xF2),
        (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0xF3),
        (NodeResidencyTier::Hot, NodeResidencyTier::Warm, 0xF4),
        (NodeResidencyTier::Warm, NodeResidencyTier::Cold, 0xF5),
    ];
    STEPS
        .iter()
        .map(|(from_tier, to_tier, tag)| {
            let request = ResidencyTransitionRequest {
                plan_id,
                node_id,
                from_tier: *from_tier,
                to_tier: *to_tier,
                expected_declared_revision: 1,
                idempotency_key: IdempotencyKey::from_bytes(bytes16(*tag, index)),
                transitioned_at_ms: 4_000,
            };
            let started = Instant::now();
            authority
                .record_residency_transition(request)
                .expect("record residency transition");
            started.elapsed()
        })
        .collect()
}

/// The full W31-D scale probe, parameterized by tier. `print` toggles the
/// profile line (the default-suite smoke run stays quiet); the assertions
/// are identical in every tier.
#[allow(clippy::too_many_lines)]
fn run_logical_tasknode_scale_probe(label: &str, node_count: u64, rss_ceiling: u64, print: bool) {
    assert!(node_count >= BASELINE_COUNT);

    // ---- Phase 0: baseline database (100 nodes, identical shapes). ------
    let baseline_database = TestDatabase::new("baseline");
    let baseline = baseline_database.open();
    let (baseline_plan, _) = apply_revision(
        &baseline,
        BASELINE_COUNT,
        IdempotencyKey::from_bytes(bytes16(0xB0, 0)),
    );
    let (baseline_handle, _) = resolve_current(
        &baseline,
        baseline_plan,
        IdempotencyKey::from_bytes(bytes16(0xB1, 0)),
    );
    let baseline_ids: Vec<TaskNodeId> = sample_indices(BASELINE_COUNT, LAZY_SAMPLE)
        .into_iter()
        .map(|position| baseline_handle.resolved_order[usize::try_from(position).expect("fits")])
        .collect();
    let mut baseline_node_reads = inspect_node_latencies(&baseline, baseline_plan, &baseline_ids);
    baseline_node_reads.sort_unstable();
    let mut baseline_residency_reads =
        inspect_residency_latencies(&baseline, baseline_plan, &baseline_ids);
    baseline_residency_reads.sort_unstable();
    let baseline_bytes = durable_bytes(&baseline_database);
    drop(baseline);

    // ---- Phase 1: declare `node_count` logical TaskNodes (one revision,
    // one fsynced transaction), then clean-close to measure the durable
    // METADATA_ONLY footprint.
    let rss_before = sample_rss_bytes();
    let scale_database = TestDatabase::new(label);
    {
        let authority = scale_database.open();
        let (plan_id, apply_elapsed) = apply_revision(
            &authority,
            node_count,
            IdempotencyKey::from_bytes(bytes16(0xA1, 0)),
        );
        drop(authority);
        let bytes_after_apply = durable_bytes(&scale_database);
        let rss_after_declare = sample_rss_bytes();
        let per_node_after_apply = bytes_after_apply / node_count;
        assert!(
            per_node_after_apply <= MAX_DURABLE_BYTES_PER_NODE,
            "{label}: durable bytes per node after declare = {per_node_after_apply}"
        );
        if print {
            eprintln!(
                "{label} declare phase: nodes={node_count} \
                 apply_total={apply_elapsed:?} \
                 bytes_after_apply={bytes_after_apply} \
                 per_node_after_apply={per_node_after_apply} \
                 rss_after_declare={rss_after_declare:?}"
            );
        }

        // ---- Phase 2: resolver + lifecycle/residency activity at scale. --
        let authority = scale_database.open();
        let (handle, resolve_elapsed) = resolve_current(
            &authority,
            plan_id,
            IdempotencyKey::from_bytes(bytes16(0xA2, 0)),
        );
        assert_eq!(
            handle.resolved_order.len(),
            usize::try_from(node_count).expect("fits")
        );
        let expected_edges = expected_edge_count(node_count);
        assert_eq!(
            handle.resolved_edges.len(),
            usize::try_from(expected_edges).expect("fits")
        );

        // Batch resolved-view readback (O(population) by design): recorded,
        // never laziness-asserted. Also harvests the index → node_id map.
        let resolved_view_started = Instant::now();
        let resolved_view = authority
            .inspect_resolved_nodes(handle.resolution_id)
            .expect("inspect resolved nodes");
        let resolved_view_elapsed = resolved_view_started.elapsed();
        assert_eq!(
            resolved_view.len(),
            usize::try_from(node_count).expect("fits")
        );
        assert_eq!(resolved_view[0].position, 1);
        assert_eq!(
            resolved_view.last().expect("non-empty").position,
            node_count
        );
        let by_index: HashMap<u64, TaskNodeId> = resolved_view
            .iter()
            .map(|node| (index_of(node.node_key), node.node_id))
            .collect();

        // Laziness faces: scattered key-scoped point reads vs the baseline.
        let scale_ids: Vec<TaskNodeId> = sample_indices(node_count, LAZY_SAMPLE)
            .into_iter()
            .map(|index| by_index[&index])
            .collect();
        let mut scale_node_reads = inspect_node_latencies(&authority, plan_id, &scale_ids);
        scale_node_reads.sort_unstable();
        let mut scale_residency_reads =
            inspect_residency_latencies(&authority, plan_id, &scale_ids);
        scale_residency_reads.sort_unstable();

        let baseline_node_p95 = percentile(&baseline_node_reads, 9_500);
        let scale_node_p95 = percentile(&scale_node_reads, 9_500);
        assert!(
            scale_node_p95 <= baseline_node_p95.saturating_mul(LAZY_RATIO),
            "{label}: inspect_node p95 regressed with population: baseline \
             {baseline_node_p95:?}, scale {scale_node_p95:?}"
        );
        assert!(
            scale_node_p95 < ABSOLUTE_LAZY_CEILING,
            "{label}: inspect_node p95 = {scale_node_p95:?}"
        );
        let baseline_residency_p95 = percentile(&baseline_residency_reads, 9_500);
        let scale_residency_p95 = percentile(&scale_residency_reads, 9_500);
        assert!(
            scale_residency_p95 <= baseline_residency_p95.saturating_mul(LAZY_RATIO),
            "{label}: residency p95 regressed with population: baseline \
             {baseline_residency_p95:?}, scale {scale_residency_p95:?}"
        );
        assert!(
            scale_residency_p95 < ABSOLUTE_LAZY_CEILING,
            "{label}: residency p95 = {scale_residency_p95:?}"
        );

        // Lifecycle mix: eligible sample over roots, the rest stay DECLARED.
        let eligible_nodes: Vec<(u64, TaskNodeId)> = root_indices(node_count, LAZY_SAMPLE)
            .into_iter()
            .map(|index| (index, by_index[&index]))
            .collect();
        let eligible_count = eligible_nodes.len() as u64;
        let (eligible_total, eligible_worst) =
            record_eligible_transitions(&authority, plan_id, &eligible_nodes);

        // Residency tiers via rehydrate + evict: the sampled nodes walk up
        // to HOT and back down to COLD; their bounded metadata facts must
        // survive eviction bit-identically (G2 posture).
        let evict_nodes: Vec<(u64, TaskNodeId)> = sample_indices(node_count, EVICT_SAMPLE)
            .into_iter()
            .map(|index| (index, by_index[&index]))
            .collect();
        let before_walk: Vec<_> = evict_nodes
            .iter()
            .map(|(_, node_id)| {
                authority
                    .inspect_node(plan_id, *node_id)
                    .expect("inspect node before walk")
                    .expect("declared node reads back")
            })
            .collect();
        let evict_walk_started = Instant::now();
        let mut tier_step_worst = Duration::ZERO;
        for (index, node_id) in &evict_nodes {
            for elapsed in walk_residency_chain(&authority, plan_id, *index, *node_id) {
                tier_step_worst = tier_step_worst.max(elapsed);
            }
        }
        let evict_walk_elapsed = evict_walk_started.elapsed();
        for (position, (_, node_id)) in evict_nodes.iter().enumerate() {
            let view = authority
                .inspect_node_residency(plan_id, *node_id)
                .expect("inspect residency after walk")
                .expect("declared node reads back");
            assert_eq!(view.tier, NodeResidencyTier::Cold);
            assert_eq!(view.transition_count, 5);
            assert!(view.last_voucher.is_some());
            let after = authority
                .inspect_node(plan_id, *node_id)
                .expect("inspect node after walk")
                .expect("declared node reads back");
            let before = &before_walk[position];
            assert_eq!(after.node_key, before.node_key);
            assert_eq!(after.kind, before.kind);
            assert_eq!(after.declared_revision, before.declared_revision);
            assert_eq!(after.node_digest, before.node_digest);
        }
        let chain = authority
            .verify_revision_chain(plan_id)
            .expect("verify revision chain");
        assert_eq!(chain.revision_count, 1);
        drop(authority);

        // ---- Bounds: durable footprint and RSS (hard ceilings). ---------
        let bytes_after_activity = durable_bytes(&scale_database);
        let per_node_after_activity = bytes_after_activity / node_count;
        assert!(
            per_node_after_activity <= MAX_DURABLE_BYTES_PER_NODE,
            "{label}: durable bytes per node after activity = {per_node_after_activity}"
        );
        let activity_delta = bytes_after_activity.saturating_sub(bytes_after_apply);
        let activity_bound = node_count * MAX_ACTIVITY_BYTES_PER_NODE + ACTIVITY_BYTES_SLACK;
        assert!(
            activity_delta <= activity_bound,
            "{label}: resolver/voucher activity added {activity_delta} bytes \
             (bound {activity_bound})"
        );
        let rss_after_activity = sample_rss_bytes();
        if let (Some(before), Some(after)) = (rss_before, rss_after_activity) {
            let delta = after.saturating_sub(before);
            assert!(
                delta <= rss_ceiling,
                "{label}: RSS delta across declare + resolver activity = {delta} \
                 (ceiling {rss_ceiling})"
            );
        }

        if print {
            eprintln!(
                "{label} logical TaskNode profile (single platform): nodes={node_count} \
                 edges={expected_edges} apply_total={apply_elapsed:?} \
                 resolve_total={resolve_elapsed:?} \
                 resolved_view_total={resolved_view_elapsed:?} \
                 inspect_p50_base={:?} inspect_p95_base={baseline_node_p95:?} \
                 inspect_p95_scale={scale_node_p95:?} \
                 inspect_max_scale={:?} \
                 residency_p50_base={:?} residency_p95_base={baseline_residency_p95:?} \
                 residency_p95_scale={scale_residency_p95:?} \
                 residency_max_scale={:?} \
                 eligible_sample={eligible_count} eligible_total={eligible_total:?} \
                 eligible_max={eligible_worst:?} \
                 evict_sample={EVICT_SAMPLE} evict_walk_total={evict_walk_elapsed:?} \
                 tier_step_max={tier_step_worst:?} \
                 baseline_database_bytes={baseline_bytes} \
                 bytes_after_apply={bytes_after_apply} \
                 bytes_after_activity={bytes_after_activity} \
                 per_node_after_apply={per_node_after_apply} \
                 per_node_after_activity={per_node_after_activity} \
                 activity_delta={activity_delta} \
                 rss_before={rss_before:?} rss_after_declare={rss_after_declare:?} \
                 rss_after_activity={rss_after_activity:?}",
                baseline_node_reads[baseline_node_reads.len() / 2],
                scale_node_reads[scale_node_reads.len() - 1],
                baseline_residency_reads[baseline_residency_reads.len() / 2],
                scale_residency_reads[scale_residency_reads.len() - 1],
            );
        }
    }
}

#[test]
fn declaration_shapes_are_deterministic_mixed_and_acyclic() {
    let count = 1_000_u64;
    let first = declaration_set(count);
    let second = declaration_set(count);
    assert_eq!(first, second, "generator must be deterministic");

    let edges: u64 = first
        .iter()
        .map(|node| u64::try_from(node.dependency_keys.len()).expect("fits"))
        .sum();
    assert_eq!(edges, expected_edge_count(count));

    let roots = first
        .iter()
        .filter(|node| node.dependency_keys.is_empty())
        .count();
    assert_eq!(
        u64::try_from(roots).expect("fits"),
        count.div_ceil(10) - (count - 1) / 100
    );

    let keys: std::collections::HashSet<[u8; 16]> =
        first.iter().map(|node| node.node_key).collect();
    assert_eq!(keys.len(), usize::try_from(count).expect("fits"));
    for node in &first {
        for dependency in &node.dependency_keys {
            assert!(keys.contains(dependency), "dependency must be declared");
            assert_ne!(*dependency, node.node_key, "no self-dependency");
            assert!(
                index_of(*dependency) < index_of(node.node_key),
                "edges point strictly backwards (acyclic by construction)"
            );
        }
    }
}

/// Default-suite smoke: the full probe pipeline at a small tier keeps every
/// helper and hard bound exercised without materializing the 10K/100K
/// databases.
#[test]
fn small_tier_probe_pipeline_smoke() {
    run_logical_tasknode_scale_probe("smoke-500", 500, 256 * 1024 * 1024, false);
}

#[test]
#[ignore = "explicit ROAD-B-004 B4-9 10K logical TaskNode scale probe"]
fn ten_thousand_logical_task_nodes_stay_lazy_and_bounded() {
    run_logical_tasknode_scale_probe("10K", 10_000, 256 * 1024 * 1024, true);
}

#[test]
#[ignore = "explicit ROAD-B-004 B4-9 100K logical TaskNode scale probe"]
fn one_hundred_thousand_logical_task_nodes_stay_lazy_and_bounded() {
    run_logical_tasknode_scale_probe("100K", 100_000, 768 * 1024 * 1024, true);
}
