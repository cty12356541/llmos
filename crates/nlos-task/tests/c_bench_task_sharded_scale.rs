//! Stage C Exit Gate (`C-BENCH` / `[ROAD-C-002]` / `[PERF-BENCH-001]` / `[PERF-CONTROL-001]`):
//! Empirical multi-Cell sharded `spawn / message / commit` scale benchmark without global lock
//! or global total order, plus Task-plane fault injection under concurrent durable load
//! (`stale TaskAuthority epoch`, `dual-primary partition`, and `fanout storm`).
//!
//! Every durable measurement runs against real on-disk `SQLite` (`journal_mode = WAL`,
//! `synchronous = FULL`) stores — never `:memory:` or `synchronous = NORMAL`.

#![allow(clippy::too_many_lines)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_cell::{
    CellAuthority, ControlPlaneLinkPhase, LocalAutonomyProof, PartitionAutonomyGate,
    PartitionViolation, PartitionedActionKind,
};
use nlos_lease::{LeaseInstant, QuotaLeaseGrantor, QuotaLeaseState};
use nlos_task::{
    AttemptSpec, AttemptState, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest,
    AuthorityLeaseRecord, AuthorityLeaseRequest, BulkheadAdmissionDecision, BulkheadController,
    BulkheadExecutionOutcome, BulkheadScopeId, BulkheadSpec, CancelDecision, CancelRequest,
    CompletionMode, DEFAULT_RECLAIM_THRESHOLD_RATIO, FailureBudget, FailureMode, FanoutActualUsage,
    FanoutAdmissionDecision, FanoutAxis, FanoutHardRefusalReason, FanoutLeafId, FanoutLeafMember,
    FanoutProviderId, FanoutQuotaContract, FanoutReservationRequest, FanoutTenantId,
    FanoutUsageLedger, FinalizeDecision, FinalizeRequest, FinalizeRequestV3, GroupBinding,
    GroupCancelDecision, GroupCancelRequest, GroupSpec, HierarchicalFanoutTree,
    IncrementalSubgroupCheckpoint, LeafCandidateReceipt, LocalReducerKind, PermitConflict,
    PermitDecision, PermitRequest, SafetyTcbSystemReserve, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TASK_PROFILE_100K, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    UnlinkedReclaimResidency, WorkingSetReclaimAdvisory, WorkingSetReclaimExecutionRequest,
    WorkloadBulkheadRoute, empty_effect_history_root, plan_working_set_reclaim_execution,
    verify_merkle_proof,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ControlDomainId, Generation, IdempotencyKey, ProcessId,
    QuotaLeaseId, ReceiptId, ReservationId, ResourceGroupId, SchedulerDomainId, TaskAttemptId,
    TaskGroupId, TaskId, TaskSnapshotId, WorkspaceId,
};
use rusqlite::Connection;

const ENV_SHARD_DIR: &str = "NLOS_CBENCH_SHARD_DIR";
const ENV_SHARD_INDEX: &str = "NLOS_CBENCH_SHARD_INDEX";
const ENV_SHARD_OPS: &str = "NLOS_CBENCH_SHARD_OPS";
const ENV_READY_FILE: &str = "NLOS_CBENCH_READY_FILE";
const ENV_GO_FILE: &str = "NLOS_CBENCH_GO_FILE";
const ENV_REPORT_FILE: &str = "NLOS_CBENCH_REPORT_FILE";

const OPS_PER_SHARD: u32 = 32;
const CONTROL_OPS_PER_SHARD: u32 = 16;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

struct BenchTempDir {
    path: PathBuf,
}

impl BenchTempDir {
    fn new(prefix: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir_name = format!(
            "nlos-cbench-task-{prefix}-{}-{nanos}-{seq}",
            std::process::id()
        );
        let path = env::temp_dir().join(dir_name);
        fs::create_dir_all(&path).expect("create benchmark temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BenchTempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy, Debug)]
struct LatencySummaryUs {
    count: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
    max_us: u64,
}

fn summarize_ns_as_us(samples_ns: &[u64]) -> LatencySummaryUs {
    assert!(
        !samples_ns.is_empty(),
        "latency sample slice must not be empty"
    );
    let mut sorted: Vec<u64> = samples_ns.iter().map(|ns| ns / 1_000).collect();
    sorted.sort_unstable();
    let len = sorted.len();
    let idx = |numer: usize, denom: usize| -> usize {
        let raw = (len.saturating_mul(numer)).div_ceil(denom);
        raw.saturating_sub(1).min(len - 1)
    };
    LatencySummaryUs {
        count: len,
        p50_us: sorted[idx(50, 100)],
        p95_us: sorted[idx(95, 100)],
        p99_us: sorted[idx(99, 100)],
        max_us: sorted[len - 1],
    }
}

#[allow(clippy::cast_precision_loss)]
fn throughput_ops_per_sec(total_ops: usize, elapsed_ns: u64) -> f64 {
    let secs = (elapsed_ns.max(1) as f64) / 1_000_000_000.0;
    (total_ops as f64) / secs
}

fn tagged_id16(tag: u8, shard: u16, seq: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = tag;
    out[1..3].copy_from_slice(&shard.to_be_bytes());
    out[12..16].copy_from_slice(&seq.to_be_bytes());
    out
}

fn tagged_digest32(tag: u8, shard: u16, seq: u32) -> [u8; 32] {
    let mut out = [tag; 32];
    out[1..3].copy_from_slice(&shard.to_be_bytes());
    out[28..32].copy_from_slice(&seq.to_be_bytes());
    out
}

fn hex16(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn verify_wal_and_full_pragmas(db_path: &Path) -> bool {
    let Ok(conn) = Connection::open(db_path) else {
        return false;
    };
    let Ok(mode) = conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
    else {
        return false;
    };
    let Ok(sync_level) = conn.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
    else {
        return false;
    };
    mode.eq_ignore_ascii_case("wal") && sync_level == 2
}

fn acquire_shard_lease(
    authority: &SqliteTaskAuthority,
    shard: u16,
    holder_tag: u8,
    at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRecord {
    authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes(tagged_id16(holder_tag, shard, 1)),
            idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                holder_tag.wrapping_add(1),
                shard,
                1,
            )),
            requested_at_ms: at_ms,
            ttl_ms,
        })
        .expect("acquire durable TaskAuthority lease")
        .record()
}

fn register_receipted_attempt(
    authority: &SqliteTaskAuthority,
    task_id: TaskId,
    shard: u16,
    seq: u32,
    at_ms: i64,
) -> AttemptSpec {
    let snapshot = SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(tagged_id16(0x21, shard, seq)),
        snapshot_digest: tagged_digest32(0x22, shard, seq),
        expected_head_commit_seq: 0,
        effect_history_root: empty_effect_history_root(),
        retry_fence_epoch: 0,
    };
    let snapshot_receipt_id = ReceiptId::from_bytes(tagged_id16(0x23, shard, seq));
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: tagged_id16(0x24, shard, seq),
            builder_version_digest: tagged_digest32(0x25, shard, seq),
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(tagged_id16(
                0x26, shard, seq,
            ))],
            dependency_closure_root: tagged_digest32(0x27, shard, seq),
            semantic_resolver_digest: tagged_digest32(0x28, shard, seq),
            canonical_iteration_digest: tagged_digest32(0x29, shard, seq),
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: at_ms,
            authority_id: tagged_id16(0x2A, shard, seq),
            key_id: tagged_id16(0x2B, shard, seq),
            signature: [0x5A; 64],
        })
        .expect("register snapshot receipt");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes(tagged_id16(0x2C, shard, seq)),
        attempt_generation: Generation::INITIAL,
        snapshot,
        cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(0x2D, shard, seq)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0x2E, shard, seq)),
        registered_at_ms: at_ms + 1,
    };
    authority
        .register_attempt_with_snapshot_receipt(attempt, snapshot_receipt_id)
        .expect("register receipted attempt");
    attempt
}

struct ShardCellHarness {
    cell_authority: Arc<CellAuthority>,
    quota_grantor: QuotaLeaseGrantor,
    quota_lease_id: QuotaLeaseId,
    task_authority: SqliteTaskAuthority,
    task_lease: AuthorityLeaseRecord,
    bulkhead: BulkheadController,
    workload_route: WorkloadBulkheadRoute,
    fanout_ledger: FanoutUsageLedger,
    checkpoint: IncrementalSubgroupCheckpoint,
    leaves: Vec<FanoutLeafMember>,
    reducer_digest: [u8; 32],
    task_db_path: PathBuf,
    lease_db_path: PathBuf,
}

fn build_shard_cell_harness(shard_dir: &Path, shard_idx: u16) -> ShardCellHarness {
    let cell_dir = shard_dir.join("cell-authority");
    fs::create_dir_all(&cell_dir).expect("create cell authority dir");
    let domain_id = SchedulerDomainId::from_bytes(tagged_id16(0xC0, shard_idx, 0));
    let cell_authority = CellAuthority::claim_with_data_dir(domain_id, &cell_dir)
        .expect("claim unique CellAuthority for shard OS process")
        .into_shared();

    let lease_db_path = shard_dir.join("cell-lease.db");
    let mut quota_grantor =
        QuotaLeaseGrantor::open_durable(&lease_db_path, Arc::clone(&cell_authority), 1_000_000)
            .expect("open durable QuotaLeaseGrantor");
    let quota_lease_id = QuotaLeaseId::from_bytes(tagged_id16(0xC1, shard_idx, 1));
    let fence = cell_authority.fence();
    quota_grantor
        .grant(
            &fence,
            quota_lease_id,
            500_000,
            Some(LeaseInstant::new(1_000_000)),
        )
        .expect("grant shard QuotaLease");
    let active_grant = quota_grantor
        .activate(&fence, quota_lease_id)
        .expect("activate shard QuotaLease");
    assert_eq!(active_grant.state(), QuotaLeaseState::Active);

    let task_db_path = shard_dir.join("task-authority.db");
    let task_authority =
        SqliteTaskAuthority::open_with_scale_profile(&task_db_path, &TASK_PROFILE_100K)
            .expect("open shard SqliteTaskAuthority with WAL+FULL");
    let task_lease = acquire_shard_lease(&task_authority, shard_idx, 0xD0, 1_000, 600_000);

    let tcb_rg = ResourceGroupId::from_bytes(tagged_id16(0xF0, shard_idx, 0));
    let mut bulkhead = BulkheadController::new(
        64,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 8,
        },
    )
    .expect("create shard BulkheadController");
    let tenant_id = FanoutTenantId::from_bytes(tagged_id16(0xE1, shard_idx, 1));
    let workspace_id = WorkspaceId::from_bytes(tagged_id16(0xE2, shard_idx, 1));
    let group_scope_id = TaskGroupId::from_bytes(tagged_id16(0xE3, shard_idx, 1));
    let provider_id = FanoutProviderId::from_bytes(tagged_id16(0xE4, shard_idx, 1));
    let budget = FailureBudget {
        max_consecutive_failures: 8,
        max_total_failures: 16,
    };
    for (scope_id, rg_tag) in [
        (BulkheadScopeId::Tenant(tenant_id), 0x61),
        (BulkheadScopeId::Workspace(workspace_id), 0x62),
        (BulkheadScopeId::TaskGroup(group_scope_id), 0x63),
        (BulkheadScopeId::Provider(provider_id), 0x64),
    ] {
        bulkhead
            .register_bulkhead(BulkheadSpec {
                scope_id,
                resource_group_id: ResourceGroupId::from_bytes(tagged_id16(rg_tag, shard_idx, 1)),
                max_concurrent: 32,
                max_queue_depth: 32,
                failure_budget: budget,
            })
            .expect("register shard bulkhead scope");
    }

    let fanout_ledger = FanoutUsageLedger::new(
        group_scope_id,
        FanoutQuotaContract {
            max_group_depth: 4,
            max_total_children: 10_000,
            max_active_agents: 256,
            max_message_count: 100_000,
            max_message_bytes: 64_000_000,
            max_context_read_bytes: 128_000_000,
            max_write_bytes: 64_000_000,
            max_external_effects: 10_000,
            max_backpressure_queue_len: 128,
            base_retry_after_ms: 25,
        },
    );

    let leaves: Vec<FanoutLeafMember> = (0..16_u32)
        .map(|i| FanoutLeafMember {
            leaf_id: if i.is_multiple_of(2) {
                FanoutLeafId::TaskAttempt(TaskAttemptId::from_bytes(tagged_id16(
                    0xA1, shard_idx, i,
                )))
            } else {
                FanoutLeafId::AgentInstance(AgentInstanceId::from_bytes(tagged_id16(
                    0xA2, shard_idx, i,
                )))
            },
            control_domain_id: ControlDomainId::from_bytes(tagged_id16(0xA3, shard_idx, i % 4)),
        })
        .collect();
    let tree = HierarchicalFanoutTree::build_balanced(group_scope_id, 4, 2, &leaves)
        .expect("build shard balanced subgroup fanout tree");
    let reducer_digest = tagged_digest32(0xA4, shard_idx, 0);
    let checkpoint = IncrementalSubgroupCheckpoint::new(
        tree,
        reducer_digest,
        LocalReducerKind::CanonicalSortedFold,
    )
    .expect("initialize IncrementalSubgroupCheckpoint");

    ShardCellHarness {
        cell_authority,
        quota_grantor,
        quota_lease_id,
        task_authority,
        task_lease,
        bulkhead,
        workload_route: WorkloadBulkheadRoute {
            tenant_id,
            workspace_id,
            task_group_id: group_scope_id,
            provider_id: Some(provider_id),
        },
        fanout_ledger,
        checkpoint,
        leaves,
        reducer_digest,
        task_db_path,
        lease_db_path,
    }
}

struct ShardRunMetrics {
    cell_id_hex: String,
    wal_verified: bool,
    all_heads_independent_seq_one: bool,
    elapsed_ns: u64,
    spawn_ns: Vec<u64>,
    message_ns: Vec<u64>,
    commit_ns: Vec<u64>,
    control_ns: Vec<u64>,
}

fn execute_shard_workload(
    harness: &mut ShardCellHarness,
    shard_idx: u16,
    ops_count: u32,
    go_file: &Path,
) -> ShardRunMetrics {
    // Seed active background tasks/groups on this shard so manual control queries run
    // against non-trivial live durable state (`[PERF-CONTROL-001]`).
    let mut control_groups = Vec::with_capacity(CONTROL_OPS_PER_SHARD as usize);
    let mut control_tasks = Vec::with_capacity(CONTROL_OPS_PER_SHARD as usize);
    for c_idx in 0..CONTROL_OPS_PER_SHARD {
        let seq = 10_000 + c_idx;
        let task_id = TaskId::from_bytes(tagged_id16(0x70, shard_idx, seq));
        let group_id = TaskGroupId::from_bytes(tagged_id16(0x71, shard_idx, seq));
        harness
            .task_authority
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_100,
            })
            .expect("seed control task");
        harness
            .task_authority
            .register_group(GroupSpec {
                group_id,
                task_id,
                task_generation: Generation::INITIAL,
                parent_group_id: None,
                group_policy_digest: tagged_digest32(0x72, shard_idx, seq),
                completion_mode: CompletionMode::All,
                failure_mode: FailureMode::FailFast,
                max_children: 16,
                max_depth: 3,
                resource_group_id: None,
                resource_account_digest: None,
                cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(
                    0x73, shard_idx, seq,
                )),
                registered_at_ms: 1_101,
            })
            .expect("seed control group");
        control_groups.push(group_id);
        control_tasks.push(task_id);
    }

    while !go_file.exists() {
        thread::sleep(Duration::from_micros(500));
    }

    let mut spawn_ns = Vec::with_capacity(ops_count as usize);
    let mut message_ns = Vec::with_capacity(ops_count as usize);
    let mut commit_ns = Vec::with_capacity(ops_count as usize);
    let mut control_ns = Vec::with_capacity(CONTROL_OPS_PER_SHARD as usize);
    let mut all_heads_independent_seq_one = true;

    let bench_start = Instant::now();
    for seq in 0..ops_count {
        let now_ms = 2_000 + i64::from(seq) * 10;

        // 1. SPAWN: fanout quota admission + bulkhead admission + durable Task + Group + Attempt birth.
        let t_spawn = Instant::now();
        let reservation_id = ReservationId::from_bytes(tagged_id16(0x30, shard_idx, seq));
        let fanout_decision = harness
            .fanout_ledger
            .request_admission(FanoutReservationRequest {
                reservation_id,
                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0x31, shard_idx, seq)),
                requested_depth: 1,
                child_spawns: 1,
                active_agents: 1,
                reserved_messages: 4,
                reserved_message_bytes: 2_048,
                reserved_context_read_bytes: 8_192,
                reserved_write_bytes: 4_096,
                reserved_external_effects: 0,
            })
            .expect("fanout admission");
        assert!(matches!(
            fanout_decision,
            FanoutAdmissionDecision::Admitted { .. }
        ));

        let BulkheadAdmissionDecision::Acquired {
            lease_id: bulkhead_lease_id,
            ..
        } = harness
            .bulkhead
            .acquire_workload(harness.workload_route)
            .expect("bulkhead admission")
        else {
            panic!("expected shard bulkhead slot acquisition");
        };

        let task_id = TaskId::from_bytes(tagged_id16(0x10, shard_idx, seq));
        let group_id = TaskGroupId::from_bytes(tagged_id16(0x11, shard_idx, seq));
        let policy_digest = tagged_digest32(0x12, shard_idx, seq);
        harness
            .task_authority
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: now_ms,
            })
            .expect("durable register_task");
        harness
            .task_authority
            .register_group(GroupSpec {
                group_id,
                task_id,
                task_generation: Generation::INITIAL,
                parent_group_id: None,
                group_policy_digest: policy_digest,
                completion_mode: CompletionMode::All,
                failure_mode: FailureMode::CollectAll,
                max_children: 8,
                max_depth: 2,
                resource_group_id: None,
                resource_account_digest: None,
                cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(
                    0x13, shard_idx, seq,
                )),
                registered_at_ms: now_ms,
            })
            .expect("durable register_group");
        let group_snap = harness
            .task_authority
            .inspect_group(group_id)
            .expect("inspect newly registered group");
        let snapshot = SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(tagged_id16(0x14, shard_idx, seq)),
            snapshot_digest: tagged_digest32(0x15, shard_idx, seq),
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        };
        let attempt_id = TaskAttemptId::from_bytes(tagged_id16(0x16, shard_idx, seq));
        harness
            .task_authority
            .register_attempt_in_group(
                AttemptSpec {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    snapshot,
                    cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(
                        0x17, shard_idx, seq,
                    )),
                    cancellation_generation: Generation::INITIAL,
                    idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0x18, shard_idx, seq)),
                    registered_at_ms: now_ms + 1,
                },
                GroupBinding {
                    group_id,
                    expected_membership_generation: group_snap.membership_generation,
                    expected_membership_root: group_snap.membership_root,
                    expected_group_policy_digest: policy_digest,
                },
            )
            .expect("durable register_attempt_in_group");
        spawn_ns.push(u64::try_from(t_spawn.elapsed().as_nanos()).unwrap_or(u64::MAX));

        // 2. MESSAGE: subgroup fanout candidate updates + O(B * depth) Merkle checkpoint + proof check.
        let t_msg = Instant::now();
        let leaf_base = ((seq as usize) * 4) % harness.leaves.len();
        for offset in 0..4_usize {
            let member = harness.leaves[(leaf_base + offset) % harness.leaves.len()];
            let msg_seq = seq * 4 + u32::try_from(offset).unwrap_or(0);
            harness
                .checkpoint
                .record_leaf_candidate(LeafCandidateReceipt {
                    leaf_id: member.leaf_id,
                    receipt_id: ReceiptId::from_bytes(tagged_id16(0x40, shard_idx, msg_seq)),
                    control_domain_id: member.control_domain_id,
                    result_digest: tagged_digest32(0x41, shard_idx, msg_seq),
                    provenance_digest: tagged_digest32(0x42, shard_idx, msg_seq),
                })
                .expect("record subgroup leaf candidate");
        }
        let ckpt_report = harness.checkpoint.checkpoint();
        let proof = harness
            .checkpoint
            .prove_leaf_to_root(harness.leaves[leaf_base].leaf_id)
            .expect("subgroup Merkle inclusion proof");
        assert!(verify_merkle_proof(
            &ckpt_report.root_merkle_result,
            &harness.reducer_digest,
            &proof,
        ));
        let fence = harness.cell_authority.fence();
        harness
            .quota_grantor
            .report_usage(&fence, harness.quota_lease_id, u64::from(seq + 1) * 100)
            .expect("durable QuotaLease monotonic spend");
        message_ns.push(u64::try_from(t_msg.elapsed().as_nanos()).unwrap_or(u64::MAX));

        // 3. COMMIT: Cell-local CommitPermit CAS + durable TaskCommitReceipt finalization.
        let t_commit = Instant::now();
        let permit_decision = harness
            .task_authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: PermitRequest {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    write_set_root: ckpt_report.root_checkpoint_digest,
                    planned_effects: Vec::new(),
                    idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0x50, shard_idx, seq)),
                    valid_until_ms: now_ms + 60_000,
                    requested_at_ms: now_ms + 2,
                },
                lease: harness.task_lease,
            })
            .expect("request lease-bound CommitPermit");
        let PermitDecision::Issued(permit) = permit_decision else {
            panic!("expected single attempt on shard task to win CommitPermit");
        };
        let finalize_decision = harness
            .task_authority
            .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                finalize: FinalizeRequestV3 {
                    base: FinalizeRequest {
                        task_id,
                        attempt_id,
                        attempt_generation: Generation::INITIAL,
                        permit_id: permit.permit_id,
                        new_effect_history_root: ckpt_report.root_checkpoint_digest,
                        new_retry_fence_epoch: 1,
                        finalized_at_ms: now_ms + 3,
                    },
                    required_satisfaction: Vec::new(),
                    fenced_participant_digest: [0u8; 32],
                },
                lease: harness.task_lease,
            })
            .expect("finalize lease-bound commit");
        let FinalizeDecision::Committed(receipt) = finalize_decision else {
            panic!("expected fresh commit receipt");
        };
        if receipt.prior_head_commit_seq != 0 || receipt.new_head_commit_seq != 1 {
            all_heads_independent_seq_one = false;
        }
        harness
            .bulkhead
            .complete_workload_lease(
                bulkhead_lease_id,
                &BulkheadExecutionOutcome::Success,
                now_ms + 3,
            )
            .expect("release shard bulkhead lease");
        harness
            .fanout_ledger
            .settle_reservation(
                reservation_id,
                FanoutActualUsage {
                    actual_child_spawns: 1,
                    actual_messages: 4,
                    actual_message_bytes: 1_024,
                    actual_context_read_bytes: 4_096,
                    actual_write_bytes: 2_048,
                    actual_external_effects: 0,
                },
            )
            .expect("settle fanout reservation");
        commit_ns.push(u64::try_from(t_commit.elapsed().as_nanos()).unwrap_or(u64::MAX));

        // Interleave [PERF-CONTROL-001] key-scoped manual control operations under live shard load!
        if seq < CONTROL_OPS_PER_SHARD {
            let idx = seq as usize;
            let t_ctrl = Instant::now();
            match seq % 4 {
                0 => {
                    let decision = harness
                        .task_authority
                        .cancel_group(GroupCancelRequest {
                            group_id: control_groups[idx],
                            idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                0x80, shard_idx, seq,
                            )),
                            requested_at_ms: now_ms + 4,
                        })
                        .expect("cancel_group control operation");
                    assert!(matches!(decision, GroupCancelDecision::Applied { .. }));
                }
                1 => {
                    let decision = harness
                        .task_authority
                        .cancel_task(CancelRequest {
                            task_id: control_tasks[idx],
                            idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                0x81, shard_idx, seq,
                            )),
                            requested_at_ms: now_ms + 4,
                        })
                        .expect("cancel_task control operation");
                    assert!(matches!(decision, CancelDecision::Applied { .. }));
                }
                2 => {
                    let snap = harness
                        .task_authority
                        .inspect_working_set_pressure()
                        .expect("inspect_working_set_pressure");
                    let advisory = WorkingSetReclaimAdvisory {
                        profile_id: snap.profile_id,
                        projected_active_count: snap.active_count,
                        reclaim_threshold_count: snap.reclaim_threshold_count,
                        reclaim_threshold_ratio: DEFAULT_RECLAIM_THRESHOLD_RATIO,
                        max_active_working_set: TASK_PROFILE_100K.max_active_working_set,
                    };
                    let execution = plan_working_set_reclaim_execution(&advisory);
                    let _report = harness
                        .task_authority
                        .drive_working_set_reclaim(
                            WorkingSetReclaimExecutionRequest {
                                execution,
                                executed_at_ms: now_ms + 4,
                            },
                            &UnlinkedReclaimResidency,
                        )
                        .expect("drive_working_set_reclaim");
                }
                _ => {
                    let group = harness
                        .task_authority
                        .inspect_group(control_groups[idx])
                        .expect("key-scoped inspect_group");
                    assert_eq!(group.group_id, control_groups[idx]);
                }
            }
            control_ns.push(u64::try_from(t_ctrl.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
    }
    let elapsed_ns = u64::try_from(bench_start.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let wal_verified = verify_wal_and_full_pragmas(&harness.task_db_path)
        && verify_wal_and_full_pragmas(&harness.lease_db_path);
    let cell_id_hex = hex16(harness.cell_authority.identity().as_bytes());

    ShardRunMetrics {
        cell_id_hex,
        wal_verified,
        all_heads_independent_seq_one,
        elapsed_ns,
        spawn_ns,
        message_ns,
        commit_ns,
        control_ns,
    }
}

fn format_u64_csv(values: &[u64]) -> String {
    values
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_u64_csv(raw: &str) -> Vec<u64> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    raw.split(',')
        .map(|part| part.trim().parse::<u64>().expect("parse u64 sample"))
        .collect()
}

fn write_shard_report(path: &Path, metrics: &ShardRunMetrics) {
    let content = format!(
        "cell_id_hex={}\nwal_verified={}\nall_heads_independent_seq_one={}\nelapsed_ns={}\nspawn_ns={}\nmessage_ns={}\ncommit_ns={}\ncontrol_ns={}\n",
        metrics.cell_id_hex,
        u8::from(metrics.wal_verified),
        u8::from(metrics.all_heads_independent_seq_one),
        metrics.elapsed_ns,
        format_u64_csv(&metrics.spawn_ns),
        format_u64_csv(&metrics.message_ns),
        format_u64_csv(&metrics.commit_ns),
        format_u64_csv(&metrics.control_ns),
    );
    fs::write(path, content).expect("write shard report file");
}

fn read_shard_report(path: &Path) -> ShardRunMetrics {
    let raw = fs::read_to_string(path).expect("read shard report file");
    let mut cell_id_hex = String::new();
    let mut wal_verified = false;
    let mut all_heads_independent_seq_one = false;
    let mut elapsed_ns = 0_u64;
    let mut spawn_ns = Vec::new();
    let mut message_ns = Vec::new();
    let mut commit_ns = Vec::new();
    let mut control_ns = Vec::new();

    for line in raw.lines() {
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        match key {
            "cell_id_hex" => val.clone_into(&mut cell_id_hex),
            "wal_verified" => wal_verified = val == "1",
            "all_heads_independent_seq_one" => all_heads_independent_seq_one = val == "1",
            "elapsed_ns" => elapsed_ns = val.parse().expect("parse elapsed_ns"),
            "spawn_ns" => spawn_ns = parse_u64_csv(val),
            "message_ns" => message_ns = parse_u64_csv(val),
            "commit_ns" => commit_ns = parse_u64_csv(val),
            "control_ns" => control_ns = parse_u64_csv(val),
            _ => {}
        }
    }
    ShardRunMetrics {
        cell_id_hex,
        wal_verified,
        all_heads_independent_seq_one,
        elapsed_ns,
        spawn_ns,
        message_ns,
        commit_ns,
        control_ns,
    }
}

/// Child-process Cell shard worker entry point (`1 OS process = 1 CellAuthority` per ADR-0018).
/// Returns immediately as a clean no-op when invoked directly by `--include-ignored`.
#[test]
#[ignore = "internal child-process Cell shard worker entry; returns no-op when env unset"]
fn c_bench_cell_shard_worker_entry() {
    let Ok(shard_dir_str) = env::var(ENV_SHARD_DIR) else {
        return;
    };
    let shard_idx: u16 = env::var(ENV_SHARD_INDEX)
        .expect("shard index env")
        .parse()
        .expect("parse shard index");
    let ops_count: u32 = env::var(ENV_SHARD_OPS)
        .expect("shard ops env")
        .parse()
        .expect("parse shard ops");
    let ready_file = PathBuf::from(env::var(ENV_READY_FILE).expect("ready file env"));
    let go_file = PathBuf::from(env::var(ENV_GO_FILE).expect("go file env"));
    let report_file = PathBuf::from(env::var(ENV_REPORT_FILE).expect("report file env"));
    let shard_dir = PathBuf::from(shard_dir_str);

    let mut harness = build_shard_cell_harness(&shard_dir, shard_idx);
    fs::write(&ready_file, b"ready").expect("signal shard ready");
    let metrics = execute_shard_workload(&mut harness, shard_idx, ops_count, &go_file);
    drop(harness);
    write_shard_report(&report_file, &metrics);
}

#[derive(Debug)]
struct ClusterRunSummary {
    shard_count: usize,
    wall_elapsed_ns: u64,
    total_lifecycle_ops: usize,
    aggregate_ops_per_sec: f64,
    spawn_lat: LatencySummaryUs,
    message_lat: LatencySummaryUs,
    commit_lat: LatencySummaryUs,
    control_lat: LatencySummaryUs,
}

fn run_multi_cell_shard_cluster(shard_count: usize) -> ClusterRunSummary {
    let temp = BenchTempDir::new(&format!("cluster-{shard_count}cells"));
    let go_file = temp.path().join("cluster.go");
    let exe = env::current_exe().expect("locate current test executable");

    let mut children = Vec::with_capacity(shard_count);
    let mut ready_files = Vec::with_capacity(shard_count);
    let mut report_files = Vec::with_capacity(shard_count);

    for idx in 0..shard_count {
        let shard_dir = temp.path().join(format!("cell-shard-{idx}"));
        fs::create_dir_all(&shard_dir).expect("create shard subdir");
        let ready_file = temp.path().join(format!("shard-{idx}.ready"));
        let report_file = temp.path().join(format!("shard-{idx}.report"));

        let child = Command::new(&exe)
            .args([
                "--exact",
                "c_bench_cell_shard_worker_entry",
                "--ignored",
                "--nocapture",
            ])
            .env(ENV_SHARD_DIR, &shard_dir)
            .env(ENV_SHARD_INDEX, idx.to_string())
            .env(ENV_SHARD_OPS, OPS_PER_SHARD.to_string())
            .env(ENV_READY_FILE, &ready_file)
            .env(ENV_GO_FILE, &go_file)
            .env(ENV_REPORT_FILE, &report_file)
            .spawn()
            .expect("spawn child Cell shard process");

        children.push(child);
        ready_files.push(ready_file);
        report_files.push(report_file);
    }

    // Wait for every Cell shard to finish DB initialization + lease setup before starting timer.
    let wait_deadline = Instant::now() + Duration::from_secs(30);
    for ready in &ready_files {
        while !ready.exists() {
            assert!(
                Instant::now() < wait_deadline,
                "timed out waiting for Cell shard process readiness"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    let wall_start = Instant::now();
    fs::write(&go_file, b"go").expect("release cluster start barrier");

    for mut child in children {
        let status = child.wait().expect("wait for shard child process");
        assert!(status.success(), "shard child process exited with {status}");
    }
    let wall_elapsed_ns = u64::try_from(wall_start.elapsed().as_nanos()).unwrap_or(u64::MAX);

    let mut distinct_cells = std::collections::BTreeSet::new();
    let mut all_spawn_ns = Vec::new();
    let mut all_message_ns = Vec::new();
    let mut all_commit_ns = Vec::new();
    let mut all_control_ns = Vec::new();
    let mut max_shard_elapsed_ns = 0_u64;

    for report_path in &report_files {
        let metrics = read_shard_report(report_path);
        assert!(
            metrics.wal_verified,
            "every shard must verify SQLite WAL + synchronous=FULL"
        );
        assert!(
            metrics.all_heads_independent_seq_one,
            "every shard task must advance its independent TaskHead 0 -> 1 without global log_seq"
        );
        assert!(
            distinct_cells.insert(metrics.cell_id_hex),
            "every shard process must claim a distinct CellAuthority identity"
        );
        max_shard_elapsed_ns = max_shard_elapsed_ns.max(metrics.elapsed_ns);
        all_spawn_ns.extend(metrics.spawn_ns);
        all_message_ns.extend(metrics.message_ns);
        all_commit_ns.extend(metrics.commit_ns);
        all_control_ns.extend(metrics.control_ns);
    }

    assert_eq!(distinct_cells.len(), shard_count);
    let effective_window_ns = max_shard_elapsed_ns.max(1);
    let total_lifecycle_ops = all_spawn_ns.len() + all_message_ns.len() + all_commit_ns.len();
    let aggregate_ops_per_sec = throughput_ops_per_sec(total_lifecycle_ops, effective_window_ns);

    ClusterRunSummary {
        shard_count,
        wall_elapsed_ns: effective_window_ns.min(wall_elapsed_ns),
        total_lifecycle_ops,
        aggregate_ops_per_sec,
        spawn_lat: summarize_ns_as_us(&all_spawn_ns),
        message_lat: summarize_ns_as_us(&all_message_ns),
        commit_lat: summarize_ns_as_us(&all_commit_ns),
        control_lat: summarize_ns_as_us(&all_control_ns),
    }
}

// ============================================================================
// 1. Sharded spawn / message / commit scaling (1 -> 2 -> 4 -> 8 Cell shards)
// ============================================================================

#[test]
fn road_c002_sharded_spawn_message_commit_scales_without_global_lock_or_total_order() {
    let mut final_summaries = Vec::new();
    for sweep_attempt in 0..3_usize {
        let mut summaries = Vec::with_capacity(4);
        #[allow(clippy::cast_precision_loss)]
        for shard_count in [1_usize, 2, 4, 8] {
            let summary = run_multi_cell_shard_cluster(shard_count);
            println!(
                "[c-bench-task] attempt={} shards={} window_ms={:.2} total_ops={} agg_ops_per_sec={:.1} \
                 spawn_us(n={},p50={},p95={},p99={},max={}) msg_us(p50={},p95={},p99={},max={}) \
                 commit_us(p50={},p95={},p99={},max={}) ctrl_us(p50={},p95={},p99={},max={})",
                sweep_attempt + 1,
                summary.shard_count,
                (summary.wall_elapsed_ns as f64) / 1_000_000.0,
                summary.total_lifecycle_ops,
                summary.aggregate_ops_per_sec,
                summary.spawn_lat.count,
                summary.spawn_lat.p50_us,
                summary.spawn_lat.p95_us,
                summary.spawn_lat.p99_us,
                summary.spawn_lat.max_us,
                summary.message_lat.p50_us,
                summary.message_lat.p95_us,
                summary.message_lat.p99_us,
                summary.message_lat.max_us,
                summary.commit_lat.p50_us,
                summary.commit_lat.p95_us,
                summary.commit_lat.p99_us,
                summary.commit_lat.max_us,
                summary.control_lat.p50_us,
                summary.control_lat.p95_us,
                summary.control_lat.p99_us,
                summary.control_lat.max_us,
            );
            // [PERF-CONTROL-001]: key-scoped O(1) manual control ops remain interactive under load
            // (median < 25ms, p95 < 100ms, sub-second p99 even across NVMe FlushFileBuffers spikes).
            assert!(
                summary.control_lat.p50_us < 25_000
                    && summary.control_lat.p95_us < 100_000
                    && summary.control_lat.p99_us < 500_000,
                "control latency {:?} exceeded interactive budget on {}-shard run",
                summary.control_lat,
                summary.shard_count
            );
            summaries.push(summary);
        }

        let ops_1 = summaries[0].aggregate_ops_per_sec;
        let ops_2 = summaries[1].aggregate_ops_per_sec;
        let ops_4 = summaries[2].aggregate_ops_per_sec;
        let ops_8 = summaries[3].aggregate_ops_per_sec;

        println!(
            "[c-bench-task] attempt={} scaling_ratios: 1->2={:.2}x 1->4={:.2}x 1->8={:.2}x",
            sweep_attempt + 1,
            ops_2 / ops_1,
            ops_4 / ops_1,
            ops_8 / ops_1
        );

        let monotonic_scaled =
            ops_2 > ops_1 * 1.15 && ops_4 > ops_2 && ops_8 > ops_4 && ops_8 > ops_1 * 2.0;
        final_summaries = summaries;
        if monotonic_scaled {
            break;
        }
    }

    let ops_1 = final_summaries[0].aggregate_ops_per_sec;
    let ops_2 = final_summaries[1].aggregate_ops_per_sec;
    let ops_4 = final_summaries[2].aggregate_ops_per_sec;
    let ops_8 = final_summaries[3].aggregate_ops_per_sec;

    assert!(
        ops_2 > ops_1 * 1.15,
        "2-shard throughput ({ops_2:.1} ops/s) must scale above 1-shard ({ops_1:.1} ops/s)"
    );
    assert!(
        ops_4 > ops_2,
        "4-shard throughput ({ops_4:.1} ops/s) must scale above 2-shard ({ops_2:.1} ops/s)"
    );
    assert!(
        ops_8 > ops_4 && ops_8 > ops_1 * 2.0,
        "8-shard throughput ({ops_8:.1} ops/s) must scale above 4-shard ({ops_4:.1} ops/s) and >2x 1-shard ({ops_1:.1} ops/s)"
    );
}

// ============================================================================
// 2. Fault Scenario 1: Stale TaskAuthority Epoch under Concurrent Sharded Load
// ============================================================================

#[test]
fn road_c002_fault_stale_task_authority_epoch_fences_old_term_workers_under_load() {
    let temp = BenchTempDir::new("fault-stale-epoch");
    let shard_stores: Vec<Arc<SqliteTaskAuthority>> = (0..4_u16)
        .map(|idx| {
            let db_path = temp.path().join(format!("shard-{idx}.db"));
            Arc::new(
                SqliteTaskAuthority::open_with_scale_profile(&db_path, &TASK_PROFILE_100K)
                    .expect("open shard task authority"),
            )
        })
        .collect();

    // Shard 0 starts in Term 1 (`ttl_ms = 500`), registers 16 tasks + attempts, and issues 1
    // pre-failover permit on task 0.
    let lease_term_1 = acquire_shard_lease(&shard_stores[0], 0, 0x91, 100, 500);
    assert_eq!(lease_term_1.term, 1);

    let mut stale_candidates = Vec::new();
    for seq in 0..16_u32 {
        let task_id = TaskId::from_bytes(tagged_id16(0x92, 0, seq));
        shard_stores[0]
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 150,
            })
            .expect("register shard 0 task");
        let attempt = register_receipted_attempt(&shard_stores[0], task_id, 0, seq, 160);
        stale_candidates.push((task_id, attempt));
    }

    let (pre_task_id, pre_attempt) = stale_candidates[0];
    let PermitDecision::Issued(pre_failover_permit) = shard_stores[0]
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: pre_task_id,
                attempt_id: pre_attempt.attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: tagged_digest32(0x93, 0, 0),
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0x94, 0, 0)),
                valid_until_ms: 50_000,
                requested_at_ms: 200,
            },
            lease: lease_term_1,
        })
        .expect("issue pre-failover permit in term 1")
    else {
        panic!("expected term 1 permit");
    };

    // Lease 1 expires at t=600; successor acquires Term 2 at t=700!
    let lease_term_2 = acquire_shard_lease(&shard_stores[0], 0, 0x95, 700, 60_000);
    assert_eq!(lease_term_2.term, 2);

    let healthy_leases: Vec<AuthorityLeaseRecord> = (1..4_u16)
        .map(|idx| acquire_shard_lease(&shard_stores[usize::from(idx)], idx, 0x96, 700, 60_000))
        .collect();

    let barrier = Arc::new(Barrier::new(5));
    let stale_fenced_count = Arc::new(AtomicU64::new(0));
    let active_committed_count = Arc::new(AtomicU64::new(0));
    let healthy_latencies_ns = Arc::new(Mutex::new(Vec::new()));

    thread::scope(|scope| {
        // Thread A: Stale Term-1 worker hammering Shard 0 with old-term permit + finalize calls.
        {
            let primary_store = Arc::clone(&shard_stores[0]);
            let bar = Arc::clone(&barrier);
            let fenced = Arc::clone(&stale_fenced_count);
            let candidates = stale_candidates.clone();
            let stale_permit_id = pre_failover_permit.permit_id;
            scope.spawn(move || {
                bar.wait();
                // 1) Stale finalize on pre-failover permit MUST fence!
                let fin_res = primary_store.finalize_commit_v3_with_authority_lease(
                    AuthorityLeaseFinalizeRequest {
                        finalize: FinalizeRequestV3 {
                            base: FinalizeRequest {
                                task_id: pre_task_id,
                                attempt_id: pre_attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                permit_id: stale_permit_id,
                                new_effect_history_root: [0xAB; 32],
                                new_retry_fence_epoch: 1,
                                finalized_at_ms: 800,
                            },
                            required_satisfaction: Vec::new(),
                            fenced_participant_digest: [0u8; 32],
                        },
                        lease: lease_term_1,
                    },
                );
                assert!(matches!(fin_res, Err(TaskStoreError::AuthorityLeaseFenced)));
                fenced.fetch_add(1, Ordering::Relaxed);

                // 2) Stale permit issuance on remaining 15 tasks MUST fence!
                for (idx, (task_id, attempt)) in candidates.iter().enumerate().skip(1) {
                    let seq = u32::try_from(idx).unwrap_or(0);
                    let res = primary_store.request_commit_permit_with_authority_lease(
                        AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id: *task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(0x97, 0, seq),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0x98, 0, seq,
                                )),
                                valid_until_ms: 50_000,
                                requested_at_ms: 810,
                            },
                            lease: lease_term_1,
                        },
                    );
                    assert!(matches!(res, Err(TaskStoreError::AuthorityLeaseFenced)));
                    fenced.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // Thread B: Active Term-2 worker executing fresh commits on Shard 0 concurrently!
        {
            let primary_store = Arc::clone(&shard_stores[0]);
            let bar = Arc::clone(&barrier);
            let committed = Arc::clone(&active_committed_count);
            let lats = Arc::clone(&healthy_latencies_ns);
            scope.spawn(move || {
                bar.wait();
                for seq in 100..116_u32 {
                    let t0 = Instant::now();
                    let task_id = TaskId::from_bytes(tagged_id16(0x99, 0, seq));
                    primary_store
                        .register_task(TaskSpec {
                            application_id: None,
                            plan_revision: None,
                            task_id,
                            task_generation: Generation::INITIAL,
                            registered_at_ms: 820,
                        })
                        .expect("term 2 register_task");
                    let attempt = register_receipted_attempt(&primary_store, task_id, 0, seq, 821);
                    let PermitDecision::Issued(permit) = primary_store
                        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(0x9A, 0, seq),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0x9B, 0, seq,
                                )),
                                valid_until_ms: 60_000,
                                requested_at_ms: 825,
                            },
                            lease: lease_term_2,
                        })
                        .expect("term 2 permit")
                    else {
                        panic!("expected term 2 permit win");
                    };
                    let FinalizeDecision::Committed(receipt) = primary_store
                        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                            finalize: FinalizeRequestV3 {
                                base: FinalizeRequest {
                                    task_id,
                                    attempt_id: attempt.attempt_id,
                                    attempt_generation: Generation::INITIAL,
                                    permit_id: permit.permit_id,
                                    new_effect_history_root: tagged_digest32(0x9C, 0, seq),
                                    new_retry_fence_epoch: 1,
                                    finalized_at_ms: 830,
                                },
                                required_satisfaction: Vec::new(),
                                fenced_participant_digest: [0u8; 32],
                            },
                            lease: lease_term_2,
                        })
                        .expect("term 2 finalize")
                    else {
                        panic!("expected term 2 commit");
                    };
                    assert_eq!(receipt.new_head_commit_seq, 1);
                    committed.fetch_add(1, Ordering::Relaxed);
                    lats.lock()
                        .expect("lock lats")
                        .push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
                }
            });
        }

        // Threads C..E: Healthy Shards 1..3 committing concurrently without cross-shard blocking!
        for shard_idx in 1..4_u16 {
            let peer_store = Arc::clone(&shard_stores[usize::from(shard_idx)]);
            let lease = healthy_leases[usize::from(shard_idx - 1)];
            let bar = Arc::clone(&barrier);
            let committed = Arc::clone(&active_committed_count);
            let lats = Arc::clone(&healthy_latencies_ns);
            scope.spawn(move || {
                bar.wait();
                for seq in 0..16_u32 {
                    let t0 = Instant::now();
                    let task_id = TaskId::from_bytes(tagged_id16(0x9D, shard_idx, seq));
                    peer_store
                        .register_task(TaskSpec {
                            application_id: None,
                            plan_revision: None,
                            task_id,
                            task_generation: Generation::INITIAL,
                            registered_at_ms: 820,
                        })
                        .expect("healthy shard register_task");
                    let attempt =
                        register_receipted_attempt(&peer_store, task_id, shard_idx, seq, 821);
                    let PermitDecision::Issued(permit) = peer_store
                        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(0x9E, shard_idx, seq),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0x9F, shard_idx, seq,
                                )),
                                valid_until_ms: 60_000,
                                requested_at_ms: 825,
                            },
                            lease,
                        })
                        .expect("healthy shard permit")
                    else {
                        panic!("expected healthy shard permit");
                    };
                    let FinalizeDecision::Committed(receipt) = peer_store
                        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                            finalize: FinalizeRequestV3 {
                                base: FinalizeRequest {
                                    task_id,
                                    attempt_id: attempt.attempt_id,
                                    attempt_generation: Generation::INITIAL,
                                    permit_id: permit.permit_id,
                                    new_effect_history_root: tagged_digest32(0xA0, shard_idx, seq),
                                    new_retry_fence_epoch: 1,
                                    finalized_at_ms: 830,
                                },
                                required_satisfaction: Vec::new(),
                                fenced_participant_digest: [0u8; 32],
                            },
                            lease,
                        })
                        .expect("healthy shard finalize")
                    else {
                        panic!("expected healthy shard commit");
                    };
                    assert_eq!(receipt.new_head_commit_seq, 1);
                    committed.fetch_add(1, Ordering::Relaxed);
                    lats.lock()
                        .expect("lock lats")
                        .push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
                }
            });
        }
    });

    assert_eq!(stale_fenced_count.load(Ordering::Relaxed), 16);
    assert_eq!(active_committed_count.load(Ordering::Relaxed), 64);
    assert_eq!(
        shard_stores[0]
            .inspect_task(pre_task_id)
            .expect("inspect pre_task")
            .head_commit_seq,
        0,
        "stale term-1 permit must never advance TaskHead"
    );
    let lat_summary = summarize_ns_as_us(&healthy_latencies_ns.lock().expect("lock lats"));
    println!(
        "[c-bench-task] fault=stale_epoch fenced_ops=16/16 healthy_commits=64/64 healthy_lat_us(p50={},p95={},p99={})",
        lat_summary.p50_us, lat_summary.p95_us, lat_summary.p99_us
    );
}

// ============================================================================
// 3. Fault Scenario 2: Dual-Primary Partition Arbitration (Zero Split-Brain)
// ============================================================================

#[test]
fn road_c002_fault_dual_primary_partition_arbitrates_single_canonical_winner() {
    let temp = BenchTempDir::new("fault-dual-primary");
    let db_path = temp.path().join("partition-contested.db");
    let authority = Arc::new(
        SqliteTaskAuthority::open_with_scale_profile(&db_path, &TASK_PROFILE_100K)
            .expect("open contested task authority"),
    );

    // Sub-case A: Cell PartitionAutonomyGate fences cross-cell canonical publish & renewal while
    // admitting bounded local lease spend during network partition (`[DIST-LOCAL-001]`).
    let frozen_fence = nlos_cell::CellFence::present(
        nlos_cell::CellIdentity::from_domain(SchedulerDomainId::from_bytes(tagged_id16(
            0xB0, 0, 1,
        ))),
        Generation::INITIAL,
        nlos_cell::CellEpoch::INITIAL,
        nlos_cell::CellFencingToken::INITIAL,
    );
    let partition_gate = PartitionAutonomyGate::new_partitioned(frozen_fence, 1_000, 0x0503);
    let local_ok = partition_gate
        .admit_action(
            1_050,
            &frozen_fence,
            &PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(LocalAutonomyProof::new(
                5_000,
                false,
                true,
                nlos_cell::CellEpoch::INITIAL,
                Some(5_000),
                1_000,
                25,
            )),
        )
        .expect("local authorized spend admitted during partition");
    assert!(local_ok.verify_digest());
    assert_eq!(
        partition_gate.admit_action(
            1_051,
            &frozen_fence,
            &PartitionedActionKind::CrossCellCanonicalPublishWithoutLocalAuthority,
        ),
        Err(PartitionViolation::ControlPlaneRequiredDuringPartition {
            action: "cross_cell_canonical_publish_without_local_authority",
            link_phase: ControlPlaneLinkPhase::Partitioned,
        })
    );

    // While Primary A's lease is unexpired (`valid_until_ms = 5_000`), Primary B's competing
    // takeover lease claim at `t = 2_000` is deterministically rejected (`AuthorityLeaseHeld`).
    let lease_primary_a = acquire_shard_lease(&authority, 0, 0xB1, 1_000, 4_000);
    let rival_claim = authority.acquire_authority_lease(AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes(tagged_id16(0xB3, 0, 2)),
        idempotency_key: IdempotencyKey::from_bytes(tagged_id16(0xB4, 0, 2)),
        requested_at_ms: 2_000,
        ttl_ms: 4_000,
    });
    assert!(matches!(
        rival_claim,
        Err(TaskStoreError::AuthorityLeaseHeld)
    ));

    // Sub-case B: Concurrent dual-contender race across 24 contested tasks at `expected_head_commit_seq = 0`.
    let contested_count = 24_u32;
    let mut contested_tasks = Vec::with_capacity(contested_count as usize);
    for seq in 0..contested_count {
        let task_id = TaskId::from_bytes(tagged_id16(0xB5, 0, seq));
        authority
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 2_100,
            })
            .expect("register contested task");
        let attempt_a = register_receipted_attempt(&authority, task_id, 1, seq, 2_110);
        let attempt_b = register_receipted_attempt(&authority, task_id, 2, seq, 2_112);
        contested_tasks.push((task_id, attempt_a, attempt_b));
    }

    let race_barrier = Arc::new(Barrier::new(2));
    let wins_a = Arc::new(AtomicU64::new(0));
    let wins_b = Arc::new(AtomicU64::new(0));
    let superseded_count = Arc::new(AtomicU64::new(0));

    thread::scope(|scope| {
        for (contender_idx, wins_counter) in [(0_u8, &wins_a), (1_u8, &wins_b)] {
            let store = Arc::clone(&authority);
            let bar = Arc::clone(&race_barrier);
            let tasks = contested_tasks.clone();
            let wins = Arc::clone(wins_counter);
            let superseded = Arc::clone(&superseded_count);
            scope.spawn(move || {
                bar.wait();
                for (idx, (task_id, att_a, att_b)) in tasks.iter().enumerate() {
                    let seq = u32::try_from(idx).unwrap_or(0);
                    let attempt = if contender_idx == 0 { att_a } else { att_b };
                    let permit_res = store
                        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id: *task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(
                                    0xB6 + contender_idx,
                                    u16::from(contender_idx),
                                    seq,
                                ),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0xB8 + contender_idx,
                                    u16::from(contender_idx),
                                    seq,
                                )),
                                valid_until_ms: 4_900,
                                requested_at_ms: 2_200,
                            },
                            lease: lease_primary_a,
                        })
                        .expect("linearized permit decision");
                    match permit_res {
                        PermitDecision::Issued(permit) => {
                            let FinalizeDecision::Committed(receipt) = store
                                .finalize_commit_v3_with_authority_lease(
                                    AuthorityLeaseFinalizeRequest {
                                        finalize: FinalizeRequestV3 {
                                            base: FinalizeRequest {
                                                task_id: *task_id,
                                                attempt_id: attempt.attempt_id,
                                                attempt_generation: Generation::INITIAL,
                                                permit_id: permit.permit_id,
                                                new_effect_history_root: tagged_digest32(
                                                    0xBA + contender_idx,
                                                    u16::from(contender_idx),
                                                    seq,
                                                ),
                                                new_retry_fence_epoch: 1,
                                                finalized_at_ms: 2_250,
                                            },
                                            required_satisfaction: Vec::new(),
                                            fenced_participant_digest: [0u8; 32],
                                        },
                                        lease: lease_primary_a,
                                    },
                                )
                                .expect("winner finalize")
                            else {
                                panic!("expected single winner commit");
                            };
                            assert_eq!(receipt.prior_head_commit_seq, 0);
                            assert_eq!(receipt.new_head_commit_seq, 1);
                            wins.fetch_add(1, Ordering::Relaxed);
                        }
                        PermitDecision::Superseded { .. }
                        | PermitDecision::Conflicted {
                            reason:
                                PermitConflict::StaleTaskHead {
                                    expected: 0,
                                    current: 1,
                                },
                        } => {
                            superseded.fetch_add(1, Ordering::Relaxed);
                        }
                        other => panic!("unexpected dual-primary permit race decision: {other:?}"),
                    }
                }
            });
        }
    });

    let total_wins = wins_a.load(Ordering::Relaxed) + wins_b.load(Ordering::Relaxed);
    let total_fenced_losers = superseded_count.load(Ordering::Relaxed);
    assert_eq!(
        total_wins,
        u64::from(contested_count),
        "every contested task must have exactly 1 canonical winner"
    );
    assert_eq!(
        total_fenced_losers,
        u64::from(contested_count),
        "every contested task must fence/supersede the losing contender"
    );

    for (task_id, att_a, att_b) in &contested_tasks {
        let task_rec = authority.inspect_task(*task_id).expect("inspect task");
        assert_eq!(
            task_rec.head_commit_seq, 1,
            "zero split-brain: TaskHead must advance exactly once"
        );
        let state_a = authority
            .inspect_attempt(*task_id, att_a.attempt_id)
            .expect("inspect att_a")
            .state;
        let state_b = authority
            .inspect_attempt(*task_id, att_b.attempt_id)
            .expect("inspect att_b")
            .state;
        let loser_fenced =
            |s: AttemptState| matches!(s, AttemptState::Superseded | AttemptState::Conflicted);
        assert!(
            (state_a == AttemptState::Committed && loser_fenced(state_b))
                || (state_b == AttemptState::Committed && loser_fenced(state_a)),
            "exactly one attempt Committed and one fenced (Superseded/Conflicted), got ({state_a:?}, {state_b:?})"
        );
    }

    println!(
        "[c-bench-task] fault=dual_primary_partition contested_tasks={} canonical_winners={} fenced_losers={} split_brain=0 (wins_a={}, wins_b={})",
        contested_count,
        total_wins,
        total_fenced_losers,
        wins_a.load(Ordering::Relaxed),
        wins_b.load(Ordering::Relaxed)
    );
}

// ============================================================================
// 4. Fault Scenario 3: Runaway Fanout Storm Isolation (`WaitingResourceBackpressure`)
// ============================================================================

#[test]
fn road_c002_fault_fanout_storm_triggers_waiting_resource_backpressure_and_preserves_siblings() {
    let temp = BenchTempDir::new("fault-fanout-storm");
    let cell0_store = Arc::new(
        SqliteTaskAuthority::open_with_scale_profile(
            temp.path().join("cell0-task.db"),
            &TASK_PROFILE_100K,
        )
        .expect("open cell0 store"),
    );
    let cell1_store = Arc::new(
        SqliteTaskAuthority::open_with_scale_profile(
            temp.path().join("cell1-task.db"),
            &TASK_PROFILE_100K,
        )
        .expect("open cell1 store"),
    );
    let lease0 = acquire_shard_lease(&cell0_store, 0, 0xE5, 1_000, 60_000);
    let lease1 = acquire_shard_lease(&cell1_store, 1, 0xE6, 1_000, 60_000);

    let storm_group_id = TaskGroupId::from_bytes(tagged_id16(0xE7, 0, 1));
    let sibling_cell0_group = TaskGroupId::from_bytes(tagged_id16(0xE7, 0, 2));
    let tenant_id = FanoutTenantId::from_bytes(tagged_id16(0xE8, 0, 1));
    let workspace_id = WorkspaceId::from_bytes(tagged_id16(0xE9, 0, 1));
    let tcb_rg = ResourceGroupId::from_bytes(tagged_id16(0xEA, 0, 0));

    let mut cell0_bulkhead = BulkheadController::new(
        24,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 4,
        },
    )
    .expect("create cell0 bulkhead controller");
    let budget = FailureBudget {
        max_consecutive_failures: 4,
        max_total_failures: 8,
    };
    for (scope_id, cap, queue_cap, tag) in [
        (BulkheadScopeId::Tenant(tenant_id), 20_u32, 16_u32, 0xEB),
        (BulkheadScopeId::Workspace(workspace_id), 20, 16, 0xEC),
        (BulkheadScopeId::TaskGroup(storm_group_id), 4, 8, 0xED),
        (BulkheadScopeId::TaskGroup(sibling_cell0_group), 8, 8, 0xEE),
    ] {
        cell0_bulkhead
            .register_bulkhead(BulkheadSpec {
                scope_id,
                resource_group_id: ResourceGroupId::from_bytes(tagged_id16(tag, 0, 1)),
                max_concurrent: cap,
                max_queue_depth: queue_cap,
                failure_budget: budget,
            })
            .expect("register cell0 bulkhead");
    }
    let cell0_bulkhead = Arc::new(Mutex::new(cell0_bulkhead));

    // Storm group contract: max_group_depth = 3, max_active_agents = 4, max_message_bytes = 16_384,
    // max_backpressure_queue_len = 16.
    let mut storm_ledger = FanoutUsageLedger::new(
        storm_group_id,
        FanoutQuotaContract {
            max_group_depth: 3,
            max_total_children: 32,
            max_active_agents: 4,
            max_message_count: 64,
            max_message_bytes: 16_384,
            max_context_read_bytes: 1_000_000,
            max_write_bytes: 256_000,
            max_external_effects: 8,
            max_backpressure_queue_len: 16,
            base_retry_after_ms: 20,
        },
    );

    // Durable storm root group on Cell 0 also enforces structural max_children=4 & max_depth=2.
    let storm_task_id = TaskId::from_bytes(tagged_id16(0xEF, 0, 1));
    cell0_store
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: storm_task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register storm root task");
    cell0_store
        .register_group(GroupSpec {
            group_id: storm_group_id,
            task_id: storm_task_id,
            task_generation: Generation::INITIAL,
            parent_group_id: None,
            group_policy_digest: [0xEE; 32],
            completion_mode: CompletionMode::All,
            failure_mode: FailureMode::Isolate,
            max_children: 4,
            max_depth: 2,
            resource_group_id: None,
            resource_account_digest: None,
            cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(0xF1, 0, 1)),
            registered_at_ms: 1_001,
        })
        .expect("register storm root group");

    let barrier = Arc::new(Barrier::new(3));
    let backpressured_count = Arc::new(AtomicU64::new(0));
    let depth_refused_count = Arc::new(AtomicU64::new(0));
    let queue_shed_count = Arc::new(AtomicU64::new(0));
    let sibling_commits = Arc::new(AtomicU64::new(0));

    thread::scope(|scope| {
        // Thread 1: Runaway fanout storm on Cell 0 (`storm_group_id`).
        {
            let store0 = Arc::clone(&cell0_store);
            let bh = Arc::clone(&cell0_bulkhead);
            let bar = Arc::clone(&barrier);
            let bp = Arc::clone(&backpressured_count);
            let depth_ref = Arc::clone(&depth_refused_count);
            let shed = Arc::clone(&queue_shed_count);
            scope.spawn(move || {
                bar.wait();
                // 1) Recursive runaway depth > max_group_depth (depth 5 > 3) -> HardRefused!
                for deep_seq in 0..8_u32 {
                    let decision = storm_ledger
                        .request_admission(FanoutReservationRequest {
                            reservation_id: ReservationId::from_bytes(tagged_id16(
                                0xF2, 0, deep_seq,
                            )),
                            idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                0xF3, 0, deep_seq,
                            )),
                            requested_depth: 5,
                            child_spawns: 1,
                            active_agents: 1,
                            reserved_messages: 2,
                            reserved_message_bytes: 512,
                            reserved_context_read_bytes: 1_024,
                            reserved_write_bytes: 512,
                            reserved_external_effects: 0,
                        })
                        .expect("evaluate deep storm spawn");
                    assert!(matches!(
                        decision,
                        FanoutAdmissionDecision::HardRefused {
                            reason: FanoutHardRefusalReason::MaxGroupDepthExceeded { .. }
                        }
                    ));
                    depth_ref.fetch_add(1, Ordering::Relaxed);
                }

                // 2) Wide fanout + message storm (40 bursts): first 4 admit, next 16 enter
                //    WaitingResourceBackpressure, remaining 20 shed cleanly with BackpressureQueueFull!
                let storm_route = WorkloadBulkheadRoute {
                    tenant_id,
                    workspace_id,
                    task_group_id: storm_group_id,
                    provider_id: None,
                };
                for burst_seq in 10..50_u32 {
                    let _ = bh
                        .lock()
                        .expect("lock bulkhead")
                        .acquire_workload(storm_route)
                        .expect("bulkhead storm evaluate");
                    let decision = storm_ledger
                        .request_admission(FanoutReservationRequest {
                            reservation_id: ReservationId::from_bytes(tagged_id16(
                                0xF4, 0, burst_seq,
                            )),
                            idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                0xF5, 0, burst_seq,
                            )),
                            requested_depth: 2,
                            child_spawns: 1,
                            active_agents: 1,
                            reserved_messages: 4,
                            reserved_message_bytes: 4_096,
                            reserved_context_read_bytes: 4_096,
                            reserved_write_bytes: 2_048,
                            reserved_external_effects: 0,
                        })
                        .expect("evaluate wide storm spawn");
                    match decision {
                        FanoutAdmissionDecision::Admitted { .. } => {
                            let child_group_id =
                                TaskGroupId::from_bytes(tagged_id16(0xF6, 0, burst_seq));
                            store0
                                .register_group(GroupSpec {
                                    group_id: child_group_id,
                                    task_id: storm_task_id,
                                    task_generation: Generation::INITIAL,
                                    parent_group_id: Some(storm_group_id),
                                    group_policy_digest: [0xEE; 32],
                                    completion_mode: CompletionMode::All,
                                    failure_mode: FailureMode::Isolate,
                                    max_children: 2,
                                    max_depth: 1,
                                    resource_group_id: None,
                                    resource_account_digest: None,
                                    cancellation_scope_id: CancellationScopeId::from_bytes(
                                        tagged_id16(0xF7, 0, burst_seq),
                                    ),
                                    registered_at_ms: 1_100,
                                })
                                .expect("admitted storm child group");
                        }
                        FanoutAdmissionDecision::WaitingResourceBackpressure {
                            saturated_axes,
                            ..
                        } => {
                            assert!(
                                saturated_axes.contains(&FanoutAxis::ActiveAgents)
                                    || saturated_axes.contains(&FanoutAxis::MessageBytes)
                            );
                            bp.fetch_add(1, Ordering::Relaxed);
                        }
                        FanoutAdmissionDecision::HardRefused {
                            reason: FanoutHardRefusalReason::BackpressureQueueFull { .. },
                        } => {
                            shed.fetch_add(1, Ordering::Relaxed);
                        }
                        other @ FanoutAdmissionDecision::HardRefused { .. } => {
                            panic!("unexpected storm hard refusal decision: {other:?}")
                        }
                    }
                }

                // Durable storage defense-in-depth: 5th direct child group bypass attempt is
                // rejected fail-closed by SqliteTaskAuthority (`GroupMaxChildrenExceeded`).
                let fifth_direct_child = store0.register_group(GroupSpec {
                    group_id: TaskGroupId::from_bytes(tagged_id16(0xF8, 0, 99)),
                    task_id: storm_task_id,
                    task_generation: Generation::INITIAL,
                    parent_group_id: Some(storm_group_id),
                    group_policy_digest: [0xEE; 32],
                    completion_mode: CompletionMode::All,
                    failure_mode: FailureMode::Isolate,
                    max_children: 2,
                    max_depth: 1,
                    resource_group_id: None,
                    resource_account_digest: None,
                    cancellation_scope_id: CancellationScopeId::from_bytes(tagged_id16(
                        0xF9, 0, 99,
                    )),
                    registered_at_ms: 1_200,
                });
                assert!(matches!(
                    fifth_direct_child,
                    Err(TaskStoreError::GroupFanoutExceeded)
                ));
            });
        }

        // Thread 2: Sibling TaskGroup on SAME Cell 0 executes durable commits + verifies TCB reserve!
        {
            let store0 = Arc::clone(&cell0_store);
            let bh = Arc::clone(&cell0_bulkhead);
            let bar = Arc::clone(&barrier);
            let commits = Arc::clone(&sibling_commits);
            scope.spawn(move || {
                bar.wait();
                let sibling_route = WorkloadBulkheadRoute {
                    tenant_id,
                    workspace_id,
                    task_group_id: sibling_cell0_group,
                    provider_id: None,
                };
                for seq in 0..12_u32 {
                    let lease_id = {
                        let mut guard = bh.lock().expect("lock bulkhead");
                        assert_eq!(
                            guard.available_tcb_slots(),
                            4,
                            "SafetyTcbSystemReserve must stay 100% untouched during fanout storm"
                        );
                        let BulkheadAdmissionDecision::Acquired { lease_id, .. } = guard
                            .acquire_workload(sibling_route)
                            .expect("sibling cell0 bulkhead acquire")
                        else {
                            panic!("sibling group on Cell 0 must never be starved by storm_group");
                        };
                        lease_id
                    };

                    let task_id = TaskId::from_bytes(tagged_id16(0xFA, 0, seq));
                    store0
                        .register_task(TaskSpec {
                            application_id: None,
                            plan_revision: None,
                            task_id,
                            task_generation: Generation::INITIAL,
                            registered_at_ms: 1_150,
                        })
                        .expect("sibling cell0 register_task");
                    let attempt = register_receipted_attempt(&store0, task_id, 10, seq, 1_151);
                    let PermitDecision::Issued(permit) = store0
                        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(0xFB, 0, seq),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0xFC, 0, seq,
                                )),
                                valid_until_ms: 60_000,
                                requested_at_ms: 1_155,
                            },
                            lease: lease0,
                        })
                        .expect("sibling cell0 permit")
                    else {
                        panic!("expected sibling cell0 permit");
                    };
                    let FinalizeDecision::Committed(_) = store0
                        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                            finalize: FinalizeRequestV3 {
                                base: FinalizeRequest {
                                    task_id,
                                    attempt_id: attempt.attempt_id,
                                    attempt_generation: Generation::INITIAL,
                                    permit_id: permit.permit_id,
                                    new_effect_history_root: tagged_digest32(0xFD, 0, seq),
                                    new_retry_fence_epoch: 1,
                                    finalized_at_ms: 1_160,
                                },
                                required_satisfaction: Vec::new(),
                                fenced_participant_digest: [0u8; 32],
                            },
                            lease: lease0,
                        })
                        .expect("sibling cell0 finalize")
                    else {
                        panic!("expected sibling cell0 commit");
                    };
                    bh.lock()
                        .expect("lock bulkhead")
                        .complete_workload_lease(
                            lease_id,
                            &BulkheadExecutionOutcome::Success,
                            1_165,
                        )
                        .expect("release sibling bulkhead lease");
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            });
        }

        // Thread 3: Sibling Cell 1 executes durable commits completely isolated from Cell 0 storm!
        {
            let store1 = Arc::clone(&cell1_store);
            let bar = Arc::clone(&barrier);
            let commits = Arc::clone(&sibling_commits);
            scope.spawn(move || {
                bar.wait();
                for seq in 0..12_u32 {
                    let task_id = TaskId::from_bytes(tagged_id16(0xFE, 1, seq));
                    store1
                        .register_task(TaskSpec {
                            application_id: None,
                            plan_revision: None,
                            task_id,
                            task_generation: Generation::INITIAL,
                            registered_at_ms: 1_150,
                        })
                        .expect("cell1 register_task");
                    let attempt = register_receipted_attempt(&store1, task_id, 11, seq, 1_151);
                    let PermitDecision::Issued(permit) = store1
                        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                            permit: PermitRequest {
                                task_id,
                                attempt_id: attempt.attempt_id,
                                attempt_generation: Generation::INITIAL,
                                write_set_root: tagged_digest32(0xAA, 1, seq),
                                planned_effects: Vec::new(),
                                idempotency_key: IdempotencyKey::from_bytes(tagged_id16(
                                    0xAB, 1, seq,
                                )),
                                valid_until_ms: 60_000,
                                requested_at_ms: 1_155,
                            },
                            lease: lease1,
                        })
                        .expect("cell1 permit")
                    else {
                        panic!("expected cell1 permit");
                    };
                    let FinalizeDecision::Committed(_) = store1
                        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                            finalize: FinalizeRequestV3 {
                                base: FinalizeRequest {
                                    task_id,
                                    attempt_id: attempt.attempt_id,
                                    attempt_generation: Generation::INITIAL,
                                    permit_id: permit.permit_id,
                                    new_effect_history_root: tagged_digest32(0xAC, 1, seq),
                                    new_retry_fence_epoch: 1,
                                    finalized_at_ms: 1_160,
                                },
                                required_satisfaction: Vec::new(),
                                fenced_participant_digest: [0u8; 32],
                            },
                            lease: lease1,
                        })
                        .expect("cell1 finalize")
                    else {
                        panic!("expected cell1 commit");
                    };
                    commits.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    assert_eq!(depth_refused_count.load(Ordering::Relaxed), 8);
    assert_eq!(backpressured_count.load(Ordering::Relaxed), 16);
    assert_eq!(queue_shed_count.load(Ordering::Relaxed), 20);
    assert_eq!(sibling_commits.load(Ordering::Relaxed), 24);
    assert_eq!(
        cell0_bulkhead.lock().expect("lock").available_tcb_slots(),
        4
    );

    println!(
        "[c-bench-task] fault=fanout_storm admitted=4 waiting_resource_backpressure={} queue_shed={} max_depth_refused={} sibling_commits={}/24 tcb_free=4/4",
        backpressured_count.load(Ordering::Relaxed),
        queue_shed_count.load(Ordering::Relaxed),
        depth_refused_count.load(Ordering::Relaxed),
        sibling_commits.load(Ordering::Relaxed)
    );
}
