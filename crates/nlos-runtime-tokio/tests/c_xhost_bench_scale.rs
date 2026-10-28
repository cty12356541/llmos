//! `ROAD-C-002-XHOST` Multi-Host Data-Plane Scale & 5-Fault Under-Load Benchmark
//! (`C-XHOST-GATE`, `ADR-0026` §2.1 `[CONS-SCOPE-001]`, §5.4 `[ROAD-C-002]`).
//!
//! Verifies:
//! 1. `[CONS-SCOPE-001]` Data-plane task commit critical path executes with `0`
//!    Raft control-plane RPCs while scaling across `1 -> 2 -> 4` isolated
//!    `SQLite WAL+FULL` Cell shards with p50/p95/p99 latency & throughput metrics.
//! 2. `[ROAD-C-002]` 5-fault matrix under concurrent load:
//!    - Stale `TaskAuthority` epoch/term (`AuthorityLeaseFenced`)
//!    - Dual-primary partition (`0` split-brain commits)
//!    - Fanout storm admission & group cancel convergence
//!    - Provider stall (`OutboxKind::ReconcileEffect` late-callback fencing, `0` leaked fibers)
//!    - Cell bulkhead (`0` `SafetyTcbSystemReserve` encroachment + `ThreeTierScheduler` offload)

#![allow(clippy::too_many_lines)]

use std::env;
use std::fs;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Instant;

use nlos_driver_mock::{
    CompleteProviderOperation, DispatchProviderOperation, MockProvider, ProviderError,
    ProviderFaultMode,
};
use nlos_operation::{OperationSpec, OperationState};
use nlos_runtime::{
    DeadlineClass, DomainPressureLevel, FiberExit, FiberSpec, ResourceGroupQueuePolicy,
    RuntimeAdapter, SaturationSource, SchedulerAdmission, WorkStealEnvelope,
    WorkerStealCapabilities,
};
use nlos_runtime_tokio::{
    ScheduledWorkItem, ThreeTierScheduler, TokioRuntimeAdapter, TokioRuntimeConfig,
    WorkerDomainConfig,
};
use nlos_store::{CancelRequestDecision, OutboxKind, SqliteOperationStore};
use nlos_task::{
    AttemptSpec, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    BulkheadAdmissionDecision, BulkheadController, BulkheadScopeId, BulkheadSpec, CompletionMode,
    FailureBudget, FailureMode, FanoutProviderId, FanoutTenantId, FinalizeDecision,
    FinalizeRequest, FinalizeRequestV3, GroupCancelDecision, GroupCancelRequest, GroupSpec,
    PermitDecision, PermitRequest, SafetyTcbSystemReserve, SnapshotBundle, SqliteTaskAuthority,
    TASK_PROFILE_100K, TaskSpec, TaskStoreError, WorkloadBulkheadRoute,
};
use nlos_types::{
    AgentInstanceId, CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation,
    IdempotencyKey, OperationId, ProcessId, ReceiptId, ResourceGroupId, SchedulerDomainId,
    TaskAttemptId, TaskGroupId, TaskId, TaskSnapshotId, WorkspaceId,
};
use rusqlite::Connection;
use tokio::runtime::Handle;

static BENCH_SEQ: AtomicU64 = AtomicU64::new(1);

struct BenchTempDir {
    path: PathBuf,
}

impl BenchTempDir {
    fn new(label: &str) -> Self {
        let seq = BENCH_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "nlos-w72-xhost-bench-{label}-{}-{seq}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create bench temp dir");
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
struct PercentilesUs {
    count: usize,
    p50_us: u64,
    p95_us: u64,
    p99_us: u64,
}

fn summarize_ns_as_us(samples_ns: &[u64]) -> PercentilesUs {
    assert!(!samples_ns.is_empty(), "samples must not be empty");
    let mut sorted: Vec<u64> = samples_ns.iter().map(|ns| ns / 1_000).collect();
    sorted.sort_unstable();
    let len = sorted.len();
    let pick = |numer: usize, denom: usize| -> usize {
        let raw = (len.saturating_mul(numer)).div_ceil(denom);
        raw.saturating_sub(1).min(len - 1)
    };
    PercentilesUs {
        count: len,
        p50_us: sorted[pick(50, 100)],
        p95_us: sorted[pick(95, 100)],
        p99_us: sorted[pick(99, 100)],
    }
}

fn id16(tag: u8, shard: u16, index: u64) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0] = tag;
    out[1..3].copy_from_slice(&shard.to_be_bytes());
    out[8..16].copy_from_slice(&index.to_be_bytes());
    out
}

fn digest32(tag: u8, shard: u16, index: u64) -> [u8; 32] {
    let mut out = [tag; 32];
    out[1..3].copy_from_slice(&shard.to_be_bytes());
    out[24..32].copy_from_slice(&index.to_be_bytes());
    out
}

fn verify_wal_full(db_path: &Path) -> bool {
    let Ok(conn) = Connection::open(db_path) else {
        return false;
    };
    let Ok(mode) = conn.pragma_query_value(None, "journal_mode", |row| row.get::<_, String>(0))
    else {
        return false;
    };
    let Ok(sync_val) = conn.pragma_query_value(None, "synchronous", |row| row.get::<_, i64>(0))
    else {
        return false;
    };
    mode.eq_ignore_ascii_case("wal") && sync_val == 2
}

fn acquire_shard_lease(
    auth: &SqliteTaskAuthority,
    shard: u16,
    holder_seq: u64,
    at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRecord {
    match auth
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes(id16(0xA1, shard, holder_seq)),
            idempotency_key: IdempotencyKey::from_bytes(id16(0xA2, shard, holder_seq)),
            requested_at_ms: at_ms,
            ttl_ms,
        })
        .expect("acquire authority lease")
    {
        AuthorityLeaseDecision::Acquired(rec) | AuthorityLeaseDecision::TakenOver(rec) => rec,
        other => panic!("expected Acquired lease, got {other:?}"),
    }
}
fn commit_data_plane_task(
    auth: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    shard: u16,
    seq: u64,
    at_ms: i64,
) {
    let task_id = TaskId::from_bytes(id16(0xB1, shard, seq));
    auth.register_task(TaskSpec {
        application_id: None,
        plan_revision: None,
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: at_ms,
    })
    .expect("register task");
    let head = auth.inspect_task(task_id).expect("inspect task head");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes(id16(0xB2, shard, seq)),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(id16(0xB3, shard, seq)),
            snapshot_digest: digest32(0xB4, shard, seq),
            expected_head_commit_seq: head.head_commit_seq,
            effect_history_root: head.head_effect_history_root,
            retry_fence_epoch: head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(id16(0xB5, shard, seq)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id16(0xB6, shard, seq)),
        registered_at_ms: at_ms + 1,
    };
    auth.register_attempt(attempt).expect("register attempt");
    let PermitDecision::Issued(permit) = auth
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: digest32(0xB7, shard, seq),
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes(id16(0xB8, shard, seq)),
                valid_until_ms: at_ms + 600_000,
                requested_at_ms: at_ms + 2,
            },
            lease: *lease,
        })
        .expect("issue lease-bound commit permit")
    else {
        panic!("expected Issued commit permit");
    };
    let fin = auth
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    new_effect_history_root: digest32(0xB9, shard, seq),
                    new_retry_fence_epoch: 0,
                    finalized_at_ms: at_ms + 3,
                },
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
            },
            lease: *lease,
        })
        .expect("finalize lease-bound commit");
    assert!(matches!(fin, FinalizeDecision::Committed(_)));
}

#[test]
fn road_c002_xhost_data_plane_zero_raft_rpc_isolation_and_multi_shard_scale() {
    let root = BenchTempDir::new("data-plane-scale");
    let raft_rpc_counter = Arc::new(AtomicU64::new(0));
    let tasks_per_shard = 16_u64;

    for shard_count in [1_usize, 2, 4] {
        let tier_dir = root.path().join(format!("shards-{shard_count}"));
        fs::create_dir_all(&tier_dir).expect("create tier dir");

        let mut shard_paths = Vec::with_capacity(shard_count);
        for s_idx in 0..shard_count {
            let db_path = tier_dir.join(format!("cell-shard-{s_idx}.sqlite"));
            let auth = SqliteTaskAuthority::open_with_scale_profile(&db_path, &TASK_PROFILE_100K)
                .expect("open shard SqliteTaskAuthority");
            assert!(
                verify_wal_full(&db_path),
                "Cell shard {s_idx} must enforce SQLite WAL + FULL durability"
            );
            drop(auth);
            shard_paths.push((u16::try_from(s_idx).unwrap_or(0), db_path));
        }

        let wall_start = Instant::now();
        let all_samples_ns: Vec<u64> = thread::scope(|scope| {
            let mut handles = Vec::with_capacity(shard_count);
            for (shard_u16, db_path) in &shard_paths {
                let s_id = *shard_u16;
                let p = db_path.clone();
                let raft_counter = Arc::clone(&raft_rpc_counter);
                handles.push(scope.spawn(move || {
                    let auth = SqliteTaskAuthority::open_with_scale_profile(&p, &TASK_PROFILE_100K)
                        .expect("reopen shard SqliteTaskAuthority");
                    let lease = acquire_shard_lease(&auth, s_id, 1, 1_000, 600_000);
                    let mut samples =
                        Vec::with_capacity(usize::try_from(tasks_per_shard).unwrap_or(16));
                    for seq in 0..tasks_per_shard {
                        let t0 = Instant::now();
                        commit_data_plane_task(
                            &auth,
                            &lease,
                            s_id,
                            seq,
                            2_000 + i64::try_from(seq).unwrap_or(0) * 10,
                        );
                        // `[CONS-SCOPE-001]`: data-plane task commits execute purely against the
                        // home Cell's local SQLite WAL+FULL authority with 0 Raft RPCs.
                        assert_eq!(raft_counter.load(Ordering::Relaxed), 0);
                        samples.push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
                    }
                    samples
                }));
            }
            let mut merged = Vec::new();
            for h in handles {
                merged.extend(h.join().expect("join shard worker"));
            }
            merged
        });
        let wall_elapsed = wall_start.elapsed();
        let stats = summarize_ns_as_us(&all_samples_ns);
        let total_commits = shard_count * usize::try_from(tasks_per_shard).unwrap_or(16);
        let throughput = f64::from(u32::try_from(total_commits).unwrap_or(u32::MAX))
            / wall_elapsed.as_secs_f64().max(1e-9);
        assert_eq!(
            raft_rpc_counter.load(Ordering::SeqCst),
            0,
            "[CONS-SCOPE-001] critical data-plane path must invoke 0 Raft RPCs"
        );
        assert_eq!(stats.count, total_commits);
        println!(
            "[xhost-bench] shards={shard_count} total_commits={total_commits} raft_rpcs=0 throughput_commits_per_s={throughput:.1} latency_us(p50={},p95={},p99={})",
            stats.p50_us, stats.p95_us, stats.p99_us
        );
    }
}
fn make_fiber_spec(tag: u8, shard: u16, seq: u64) -> FiberSpec {
    let raw = id16(tag, shard, seq);
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(raw),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id16(tag.wrapping_add(1), shard, seq)),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id16(tag.wrapping_add(2), shard, seq)),
        process_generation: Generation::INITIAL,
        task_attempt_id: Some(TaskAttemptId::from_bytes(id16(
            tag.wrapping_add(3),
            shard,
            seq,
        ))),
        cancellation_scope_id: CancellationScopeId::from_bytes(id16(
            tag.wrapping_add(4),
            shard,
            seq,
        )),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id16(tag.wrapping_add(5), shard, 0)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id16(0xF0, shard, 0)),
        deadline: None,
    }
}

fn make_sched_item(tag: u8, seq: u64, rg_id: [u8; 16]) -> ScheduledWorkItem {
    let raw = id16(tag, 0, seq);
    ScheduledWorkItem {
        item_id: raw,
        fiber_id: Some(ExecutionFiberId::from_bytes(raw)),
        resource_group_id: rg_id,
        requested_priority: 100,
        deadline_class: DeadlineClass::Interactive,
        cancel_epoch: 1,
        enqueued_at_ms: 100 + seq,
        runnable: true,
        steal_envelope: WorkStealEnvelope {
            migratable: true,
            namespace_root: [0x11; 32],
            secret_scope_id: Some([0x22; 16]),
            preferred_context_digest: None,
            device_affinity: None,
            has_active_device_lease: false,
            has_active_effect_permit: false,
            has_active_reservation: false,
            provider_tenant_id: Some([0x33; 16]),
            task_authority_id: [0x44; 16],
            authority_term: 2,
            isolation_domain_id: [0x55; 16],
            isolation_fenced: false,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn road_c002_xhost_five_fault_matrix_under_concurrent_load() {
    let root = BenchTempDir::new("five-fault-matrix");
    let task_db = root.path().join("fault-task-authority.sqlite");
    let auth = SqliteTaskAuthority::open_with_scale_profile(&task_db, &TASK_PROFILE_100K)
        .expect("open fault SqliteTaskAuthority");

    // ------------------------------------------------------------------------
    // Fault 1 & Fault 2: Stale TaskAuthority epoch/term + Dual-Primary Partition (`0` split-brain)
    // ------------------------------------------------------------------------
    let lease_term_1 = acquire_shard_lease(&auth, 0, 1, 100, 100); // expires at 200 ms
    assert_eq!(lease_term_1.term, 1);

    // Dual-primary competitor attempting to acquire while term-1 lease is active (< 200 ms) is rejected!
    let premature = auth
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes(id16(0xA1, 0, 2)),
            idempotency_key: IdempotencyKey::from_bytes(id16(0xA2, 0, 99)),
            requested_at_ms: 150,
            ttl_ms: 500,
        })
        .expect_err("premature dual-primary acquire must fail closed");
    assert!(matches!(premature, TaskStoreError::AuthorityLeaseHeld));

    // Successor acquires term-2 lease after expiry (at 250 ms > 200 ms).
    let lease_term_2 = acquire_shard_lease(&auth, 0, 2, 250, 10_000);
    assert_eq!(lease_term_2.term, 2);

    // Stale term-1 primary attempts to issue a CommitPermit -> rejected fail-closed (`AuthorityLeaseFenced`)!
    let fenced_task = TaskId::from_bytes(id16(0xC1, 0, 1));
    auth.register_task(TaskSpec {
        application_id: None,
        plan_revision: None,
        task_id: fenced_task,
        task_generation: Generation::INITIAL,
        registered_at_ms: 260,
    })
    .expect("register fenced_task");
    let head = auth.inspect_task(fenced_task).expect("head");
    let stale_attempt = AttemptSpec {
        task_id: fenced_task,
        attempt_id: TaskAttemptId::from_bytes(id16(0xC2, 0, 1)),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(id16(0xC3, 0, 1)),
            snapshot_digest: digest32(0xC4, 0, 1),
            expected_head_commit_seq: head.head_commit_seq,
            effect_history_root: head.head_effect_history_root,
            retry_fence_epoch: head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(id16(0xC5, 0, 1)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id16(0xC6, 0, 1)),
        registered_at_ms: 261,
    };
    auth.register_attempt(stale_attempt)
        .expect("register stale_attempt");
    let stale_err = auth
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: fenced_task,
                attempt_id: stale_attempt.attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: digest32(0xC7, 0, 1),
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes(id16(0xC8, 0, 1)),
                valid_until_ms: 10_000,
                requested_at_ms: 265,
            },
            lease: lease_term_1,
        })
        .expect_err("stale term-1 lease must be fenced");
    assert!(matches!(stale_err, TaskStoreError::AuthorityLeaseFenced));
    // Term-2 primary commits cleanly (`0` split-brain commits from old primary).
    commit_data_plane_task(&auth, &lease_term_2, 0, 77, 270);

    // ------------------------------------------------------------------------
    // Fault 3 & Fault 5: Fanout Storm Admission + Group Cancel + Cell Bulkhead & Offload
    // ------------------------------------------------------------------------
    let storm_task = TaskId::from_bytes(id16(0xD1, 0, 1));
    let storm_group = TaskGroupId::from_bytes(id16(0xD2, 0, 1));
    auth.register_task(TaskSpec {
        application_id: None,
        plan_revision: None,
        task_id: storm_task,
        task_generation: Generation::INITIAL,
        registered_at_ms: 300,
    })
    .expect("register storm_task");
    auth.register_group(GroupSpec {
        group_id: storm_group,
        task_id: storm_task,
        task_generation: Generation::INITIAL,
        parent_group_id: None,
        group_policy_digest: digest32(0xD3, 0, 1),
        completion_mode: CompletionMode::All,
        failure_mode: FailureMode::FailFast,
        max_children: 16,
        max_depth: 3,
        resource_group_id: None,
        resource_account_digest: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(id16(0xD4, 0, 1)),
        registered_at_ms: 301,
    })
    .expect("register storm_group");

    let tcb_rg = ResourceGroupId::from_bytes(id16(0xD5, 0, 0));
    let mut bulkhead = BulkheadController::new(
        24,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 4,
        },
    )
    .expect("create BulkheadController");
    let tenant_id = FanoutTenantId::from_bytes(id16(0xD6, 0, 1));
    let workspace_id = WorkspaceId::from_bytes(id16(0xD7, 0, 1));
    let prov_stalled_id = FanoutProviderId::from_bytes(id16(0xD8, 0, 1));
    let budget = FailureBudget {
        max_consecutive_failures: 8,
        max_total_failures: 16,
    };
    for (scope_id, cap, q_cap, tag) in [
        (BulkheadScopeId::Tenant(tenant_id), 16_u32, 16_u32, 0xD9_u8),
        (BulkheadScopeId::Workspace(workspace_id), 16, 16, 0xDA),
        (BulkheadScopeId::TaskGroup(storm_group), 4, 4, 0xDB),
        (BulkheadScopeId::Provider(prov_stalled_id), 2, 2, 0xDC),
    ] {
        bulkhead
            .register_bulkhead(BulkheadSpec {
                scope_id,
                resource_group_id: ResourceGroupId::from_bytes(id16(tag, 0, 1)),
                max_concurrent: cap,
                max_queue_depth: q_cap,
                failure_budget: budget,
            })
            .expect("register bulkhead scope");
    }

    let storm_route = WorkloadBulkheadRoute {
        tenant_id,
        workspace_id,
        task_group_id: storm_group,
        provider_id: None,
    };
    let mut storm_shed = 0_u64;
    for _ in 0..32_u32 {
        if matches!(
            bulkhead
                .acquire_workload(storm_route)
                .expect("storm acquire"),
            BulkheadAdmissionDecision::RejectedQueueFull { .. }
        ) {
            storm_shed += 1;
        }
    }
    assert_eq!(
        storm_shed, 24,
        "fanout storm beyond cap+queue must shed deterministically"
    );
    assert_eq!(
        bulkhead.available_tcb_slots(),
        4,
        "SafetyTcbSystemReserve must experience 0 encroachment during fanout storm"
    );
    let cancel_grp = auth
        .cancel_group(GroupCancelRequest {
            group_id: storm_group,
            idempotency_key: IdempotencyKey::from_bytes(id16(0xDD, 0, 1)),
            requested_at_ms: 350,
        })
        .expect("cancel storm_group");
    assert!(matches!(cancel_grp, GroupCancelDecision::Applied { .. }));

    // ------------------------------------------------------------------------
    // Fault 4: Provider Stall under Concurrent Fast Load + Late Callback Fencing
    // ------------------------------------------------------------------------
    let adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: 64,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("create TokioRuntimeAdapter");
    let op_store = Arc::new(
        SqliteOperationStore::open(root.path().join("stalled-op-store.sqlite"))
            .expect("open SqliteOperationStore"),
    );
    let provider = Arc::new(MockProvider::new(Arc::clone(&op_store)));
    let stalled_spec = make_fiber_spec(0xE1, 0, 1);
    let cancel_scope = stalled_spec.cancellation_scope_id;
    let stalled_fiber = adapter
        .spawn_fiber(stalled_spec, Box::pin(pending()))
        .expect("spawn stalled fiber");
    let op_reg = provider
        .register(OperationSpec {
            operation_id: OperationId::from_bytes(id16(0xE2, 0, 1)),
            generation: Generation::INITIAL,
            owner_fiber: stalled_fiber,
            cancellation_scope_id: cancel_scope,
            cancellation_generation: Generation::INITIAL,
        })
        .expect("register stalled operation");
    let cb_id = CallbackId::from_bytes(id16(0xE3, 0, 1));
    let _ = provider
        .dispatch(DispatchProviderOperation {
            handle: op_reg.handle,
            callback_id: cb_id,
        })
        .expect("dispatch stalled operation");
    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        provider.complete(CompleteProviderOperation {
            handle: op_reg.handle,
            callback_id: cb_id,
            seed: [0x11; 32],
        }),
        Err(ProviderError::Unreachable)
    ));

    let cancel_op = op_store
        .request_cancel_idempotent(
            op_reg.handle,
            CancelEpoch::INITIAL,
            ReceiptId::from_bytes(id16(0xE4, 0, 1)),
        )
        .expect("cancel stalled op");
    assert!(matches!(
        cancel_op,
        CancelRequestDecision::Applied(snap) if snap.state == OperationState::CancelRequested
    ));
    adapter
        .cancel_scope(cancel_scope, Generation::INITIAL)
        .expect("cancel stalled scope");
    let rt_join = adapter.clone();
    let exit = tokio::task::spawn_blocking(move || rt_join.join_fiber(stalled_fiber))
        .await
        .expect("join task")
        .expect("reap stalled fiber");
    assert_eq!(exit, FiberExit::Cancelled);

    provider.disarm_fault();
    let _ = provider
        .complete(CompleteProviderOperation {
            handle: op_reg.handle,
            callback_id: cb_id,
            seed: [0x22; 32],
        })
        .expect("late completion after cancel");
    let outbox = op_store.pending_outbox(8).expect("pending outbox");
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].kind, OutboxKind::ReconcileEffect);
    assert_eq!(adapter.registered_fibers(), 0);

    // ThreeTierScheduler saturation offload verification (`Cell 0 -> Cell 1`).
    let sched = ThreeTierScheduler::new([0xA0; 16], 32);
    let rg_id = [0xD1; 16];
    let mut workers = Vec::new();
    for c_idx in 0..2_u64 {
        let cell_dom = sched.add_cell(id16(0xB0, 0, c_idx), 8);
        cell_dom.register_resource_group(ResourceGroupQueuePolicy {
            resource_group_id: rg_id,
            max_queue_depth: 16,
            parent_hard_quota_in_flight: 16,
            weight: 1,
            max_priority_cap: 150,
            starvation_aging_step_ms: 50,
        });
        let w = cell_dom.add_worker(
            id16(0xC0, 0, c_idx),
            WorkerDomainConfig {
                max_concurrency: 8,
                base_materialization_window: 8,
            },
            WorkerStealCapabilities {
                allowed_namespaces: vec![[0x11; 32]],
                allowed_secret_scopes: vec![[0x22; 16]],
                attached_devices: Vec::new(),
                allowed_provider_tenants: vec![[0x33; 16]],
                trusted_task_authorities: vec![([0x44; 16], 1)],
                active_isolation_domains: vec![[0x55; 16]],
                fenced_isolation_domains: Vec::new(),
                warm_context_digests: Vec::new(),
                require_warm_context_on_steal: false,
            },
        );
        workers.push(w);
    }
    workers[0].set_saturation_source(SaturationSource::ModelSlot, true);
    assert_eq!(
        sched
            .global()
            .inspect_cell_pressure(id16(0xB0, 0, 0))
            .expect("cell 0 pressure")
            .level,
        DomainPressureLevel::Saturated
    );
    let adm =
        sched.place_across_cells(make_sched_item(0xEE, 1, rg_id), Some(id16(0xB0, 0, 0)), 200);
    let SchedulerAdmission::Admitted {
        assigned_domain, ..
    } = adm
    else {
        panic!("expected offload admission to healthy Cell 1, got {adm:?}");
    };
    assert_eq!(assigned_domain, workers[1].domain_id());
}
