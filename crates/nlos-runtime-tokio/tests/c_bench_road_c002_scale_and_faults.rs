//! Stage C Exit Gate (`C-BENCH` / `[ROAD-C-002]` / `[PERF-CLASS-001]` / `[PERF-SCALE-001]` /
//! `[PERF-SCALE-002]` / `[SCALE-PROFILE-001]` / `[PERF-BENCH-001]` / `[PERF-CONTROL-001]`):
//!
//! 1. `[PERF-CLASS-001]` empirical benchmark across `Fast`, `Streaming`, and `Slow` workload
//!    classes with explicit durability barrier counts and p50/p95/p99 latencies.
//! 2. `[PERF-SCALE-001]` & `[PERF-SCALE-002]` 8-Cell sharded durable `10K_LOGICAL` and
//!    `100K_LOGICAL` scale benchmarks across `1%`, `5%`, and `10%` active working-set ratios,
//!    with strict `[SCALE-PROFILE-001]` separation of logical / unmaterialized / materialized /
//!    runnable / active-fiber / resident counts, plus `[PERF-CONTROL-001]` manual control latency.
//! 3. `[ROAD-C-002]` multi-Cell runtime & provider fault injection under concurrent load
//!    (`provider stall` and `Cell bulkhead`).
//!
//! All durable stores use real on-disk `SQLite` (`journal_mode = WAL`, `synchronous = FULL`).

#![allow(clippy::too_many_lines)]

use std::convert::Infallible;
use std::env;
use std::fs;
use std::future::pending;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_channel::{
    ChannelAuthority, ChannelDecision, CreateChannelRequest, EnqueueDecision, EnqueueRequest,
};
use nlos_driver_mock::{
    CompleteProviderOperation, DispatchProviderOperation, MockProvider, ProviderError,
    ProviderFaultMode,
};
use nlos_operation::{OperationSpec, OperationState};
use nlos_plan::{
    AdmissionConsult, AdmissionConsultOutcome, ApplyPlanRevisionRequest, MaterializationAdmission,
    MaterializationScheduler, NodeResidencyTier, PlanNodeDeclaration, PlanNodeKind,
    PlanRevisionSelector, ResidencyTransitionRequest, ResolvePlanRequest, SqlitePlanAuthority,
};
use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    ProcessAuthority, ProcessBindingDecision, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest,
};
use nlos_runtime::{
    DeadlineClass, DomainPressureLevel, FiberExit, FiberHandle, FiberSpec, FiberState,
    ResourceGroupQueuePolicy, RuntimeAdapter, SaturationSource, SchedulerAdmission,
    WorkStealEnvelope, WorkerStealCapabilities,
};
use nlos_runtime_tokio::{
    ChannelSequenceWait, DeliveryReport, ScheduledWorkItem, SnapshotResumable, ThreeTierScheduler,
    TokioRuntimeAdapter, TokioRuntimeConfig, WaitOutcome, WorkerDomainConfig,
};
use nlos_store::{CancelRequestDecision, OutboxKind, SqliteOperationStore};
use nlos_task::{
    BulkheadAdmissionDecision, BulkheadController, BulkheadExecutionOutcome, BulkheadScopeId,
    BulkheadSpec, CancelDecision, CancelRequest, CompletionMode, DEFAULT_RECLAIM_THRESHOLD_RATIO,
    FailureBudget, FailureMode, FanoutProviderId, FanoutTenantId, GroupCancelDecision,
    GroupCancelRequest, GroupSpec, SafetyTcbSystemReserve, SqliteTaskAuthority, TASK_PROFILE_100K,
    TaskSpec, UnlinkedReclaimResidency, WorkingSetReclaimAdvisory,
    WorkingSetReclaimExecutionRequest, WorkloadBulkheadRoute, plan_working_set_reclaim_execution,
};
use nlos_types::{
    AgentInstanceId, CallbackId, CancelEpoch, CancellationScopeId, ChannelId, ExecutionFiberId,
    Generation, IdempotencyKey, IsolationDomainId, OperationId, ProcessId, ReceiptId,
    ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskGroupId, TaskId, TaskNodeId, TaskPlanId,
    WorkspaceId,
};
use nlos_wait::{BindingId, NotifyCommitsRequest, RegisterWaitRequest, WaitAuthority, WaitState};
use rusqlite::Connection;
use tokio::runtime::Handle;

const CLUSTER_CELL_SHARDS: usize = 8;
const HOST_RUNTIME_THREADS: usize = 8;

const GLOBAL_DOMAIN_BYTES: [u8; 16] = [0xA0; 16];
const NS_ROOT: [u8; 32] = [0x11; 32];
const SECRET_SCOPE: [u8; 16] = [0x22; 16];
const TENANT_MAIN: [u8; 16] = [0x33; 16];
const AUTHORITY_MAIN: [u8; 16] = [0x44; 16];
const ISOLATION_MAIN: [u8; 16] = [0x55; 16];
const RG_PRIMARY: [u8; 16] = [0xD1; 16];
const RG_SECONDARY: [u8; 16] = [0xD2; 16];

static ROOT_SEQ: AtomicU64 = AtomicU64::new(1);

struct BenchRoot {
    path: PathBuf,
}

impl BenchRoot {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let seq = ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = env::temp_dir().join(format!(
            "nlos-cbench-rt-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("create bench root dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BenchRoot {
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
    max_us: u64,
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
        max_us: sorted[len - 1],
    }
}

#[allow(clippy::cast_precision_loss)]
fn ops_per_sec(count: usize, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64().max(1e-9);
    (count as f64) / secs
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

fn cell_nominal_id(shard: usize) -> [u8; 16] {
    id16(
        0xB0,
        u16::try_from(shard).unwrap_or(0),
        u64::try_from(shard).unwrap_or(0),
    )
}

fn worker_nominal_id(shard: usize, worker_slot: usize) -> [u8; 16] {
    id16(
        0xC0,
        u16::try_from(shard).unwrap_or(0),
        u64::try_from(worker_slot).unwrap_or(0),
    )
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

fn directory_durable_bytes_recursive(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut total = 0_u64;
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            total += directory_durable_bytes_recursive(&path);
        } else if meta.is_file() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.ends_with("-shm") {
                total += meta.len();
            }
        }
    }
    total
}

fn sqlite_db_and_wal_bytes(db_path: &Path) -> u64 {
    let main_bytes = fs::metadata(db_path).map_or(0, |m| m.len());
    let mut wal_os = db_path.as_os_str().to_os_string();
    wal_os.push("-wal");
    let wal_bytes = fs::metadata(PathBuf::from(wal_os)).map_or(0, |m| m.len());
    main_bytes + wal_bytes
}

/// Samples current OS process resident working-set memory in bytes (`WorkingSet64` on Windows,
/// `ps -o rss=` on macOS/Linux) without `unsafe` code.
fn sample_process_working_set_bytes() -> Option<u64> {
    let pid_str = std::process::id().to_string();
    #[cfg(target_os = "windows")]
    {
        let script = format!("(Get-Process -Id {pid_str}).WorkingSet64");
        let output = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        let output = Command::new("ps")
            .args(["-o", "rss=", "-p", &pid_str])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let kb: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
}

fn default_worker_capabilities() -> WorkerStealCapabilities {
    WorkerStealCapabilities {
        allowed_namespaces: vec![NS_ROOT],
        allowed_secret_scopes: vec![SECRET_SCOPE],
        attached_devices: Vec::new(),
        allowed_provider_tenants: vec![TENANT_MAIN],
        trusted_task_authorities: vec![(AUTHORITY_MAIN, 1)],
        active_isolation_domains: vec![ISOLATION_MAIN],
        fenced_isolation_domains: Vec::new(),
        warm_context_digests: Vec::new(),
        require_warm_context_on_steal: false,
    }
}

fn default_steal_envelope() -> WorkStealEnvelope {
    WorkStealEnvelope {
        migratable: true,
        namespace_root: NS_ROOT,
        secret_scope_id: Some(SECRET_SCOPE),
        preferred_context_digest: None,
        device_affinity: None,
        has_active_device_lease: false,
        has_active_effect_permit: false,
        has_active_reservation: false,
        provider_tenant_id: Some(TENANT_MAIN),
        task_authority_id: AUTHORITY_MAIN,
        authority_term: 2,
        isolation_domain_id: ISOLATION_MAIN,
        isolation_fenced: false,
    }
}

fn make_work_item(
    tag: u8,
    shard: u16,
    seq: u64,
    rg_id: [u8; 16],
    priority: u8,
    deadline_class: DeadlineClass,
    enqueued_at_ms: u64,
) -> ScheduledWorkItem {
    let raw_id = id16(tag, shard, seq);
    ScheduledWorkItem {
        item_id: raw_id,
        fiber_id: Some(ExecutionFiberId::from_bytes(raw_id)),
        resource_group_id: rg_id,
        requested_priority: priority,
        deadline_class,
        cancel_epoch: 1,
        enqueued_at_ms,
        runnable: true,
        steal_envelope: default_steal_envelope(),
    }
}

fn make_fiber_spec(tag: u8, shard: u16, seq: u64, generation: Generation) -> FiberSpec {
    let raw = id16(tag, shard, seq);
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(raw),
        fiber_generation: generation,
        agent_instance_id: AgentInstanceId::from_bytes(raw),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id16(tag.wrapping_add(1), shard, seq)),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(id16(
            tag.wrapping_add(2),
            shard,
            seq,
        )),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(RG_PRIMARY),
        scheduler_domain_id: SchedulerDomainId::from_bytes(cell_nominal_id(usize::from(shard))),
        deadline: None,
    }
}

// ============================================================================
// 1. [PERF-CLASS-001] Workload Classes Benchmark (`Fast` / `Streaming` / `Slow`)
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn perf_class_001_fast_streaming_and_slow_workload_classes_benchmark() {
    let root = BenchRoot::new("perf-class-001");
    let adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: 512,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("create TokioRuntimeAdapter");

    // ------------------------------------------------------------------------
    // Class A: `Fast` — Local handle/scheduler dispatch & small channel handoff
    // Durability barriers per op: 0 (pure in-memory scheduler + M:N fiber handoff)
    // ------------------------------------------------------------------------
    let fast_scheduler = ThreeTierScheduler::new(GLOBAL_DOMAIN_BYTES, 256);
    let fast_cell = fast_scheduler.add_cell(cell_nominal_id(0), 256);
    fast_cell.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_PRIMARY,
        max_queue_depth: 512,
        parent_hard_quota_in_flight: 512,
        weight: 2,
        max_priority_cap: 220,
        starvation_aging_step_ms: 50,
    });
    let fast_worker = fast_cell.add_worker(
        worker_nominal_id(0, 0),
        WorkerDomainConfig {
            max_concurrency: 256,
            base_materialization_window: 256,
        },
        default_worker_capabilities(),
    );

    let fast_ops = 256_u64;
    let mut fast_latencies_ns = Vec::with_capacity(usize::try_from(fast_ops).unwrap_or(0));
    let fast_wall_start = Instant::now();
    for seq in 0..fast_ops {
        let op_start = Instant::now();
        let item = make_work_item(0x10, 0, seq, RG_PRIMARY, 120, DeadlineClass::Immediate, seq);
        let admission = fast_cell.place_runnable(item, seq);
        assert!(admission.is_admitted());
        let dispatched = fast_worker
            .dequeue_next(seq)
            .expect("dequeue fast work item");
        let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
        let handle = adapter
            .spawn_fiber(
                make_fiber_spec(0x11, 0, seq, Generation::INITIAL),
                Box::pin(async move {
                    let _ = tx.send(seq.wrapping_mul(7));
                    FiberExit::Completed
                }),
            )
            .expect("spawn fast fiber");
        let val = rx.await.expect("fast oneshot handoff");
        assert_eq!(val, seq.wrapping_mul(7));
        fast_worker.complete_in_flight(dispatched.item.resource_group_id);
        let adapter_join = adapter.clone();
        let exit = tokio::task::spawn_blocking(move || adapter_join.join_fiber(handle))
            .await
            .expect("join blocking task")
            .expect("join fast fiber");
        assert_eq!(exit, FiberExit::Completed);
        fast_latencies_ns.push(u64::try_from(op_start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    let fast_wall = fast_wall_start.elapsed();
    let fast_stats = summarize_ns_as_us(&fast_latencies_ns);
    assert_eq!(adapter.registered_fibers(), 0);

    // ------------------------------------------------------------------------
    // Class B: `Streaming` — Bounded backpressure incremental consumption + cancel fence
    // Durability barriers: 1 durable ChannelAuthority/WaitAuthority window commit per stream
    //                      (0 synchronous disk barriers per incremental token chunk!)
    // ------------------------------------------------------------------------
    let stream_dir = root.path().join("streaming-authority");
    fs::create_dir_all(&stream_dir).expect("create stream dir");
    let channel_auth =
        Arc::new(ChannelAuthority::open(&stream_dir).expect("open ChannelAuthority"));
    let wait_auth =
        WaitAuthority::open(&stream_dir, Arc::clone(&channel_auth)).expect("open WaitAuthority");
    let ChannelDecision::Created(stream_channel) = channel_auth
        .create_channel(CreateChannelRequest {
            capacity_bytes: 65_536,
            policy_digest: [0x42; 32],
            idempotency_key: IdempotencyKey::from_bytes(id16(0x20, 0, 0)),
            created_at_ms: 1_000,
        })
        .expect("create streaming channel")
    else {
        panic!("expected fresh streaming channel");
    };

    let stream_count = 48_u64;
    let chunks_per_stream = 16_u64;
    let cancelled_streams_target = 8_u64;
    let mut stream_latencies_ns = Vec::with_capacity(usize::try_from(stream_count).unwrap_or(0));
    let mut chunk_latencies_ns = Vec::new();
    let mut fenced_cancelled_streams = 0_u64;
    let mut backpressure_yields_observed = 0_u64;

    let stream_wall_start = Instant::now();
    for s_idx in 0..stream_count {
        let s_start = Instant::now();
        let should_cancel_midstream = s_idx < cancelled_streams_target;
        // Bounded capacity = 4 enforces real producer backpressure across 16 chunks.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<[u8; 32]>(4);
        let bp_flag = Arc::new(AtomicU64::new(0));
        let bp_flag_prod = Arc::clone(&bp_flag);

        let spec = make_fiber_spec(0x21, 0, s_idx, Generation::INITIAL);
        let cancel_scope = spec.cancellation_scope_id;
        let producer_handle = adapter
            .spawn_fiber(
                spec,
                Box::pin(async move {
                    for chunk_idx in 0..chunks_per_stream {
                        if tx.capacity() == 0 {
                            bp_flag_prod.fetch_add(1, Ordering::Relaxed);
                        }
                        if tx.send(digest32(0x25, 0, chunk_idx)).await.is_err() {
                            return FiberExit::Cancelled;
                        }
                    }
                    FiberExit::Completed
                }),
            )
            .expect("spawn streaming producer fiber");

        let mut received_chunks = 0_u64;
        while received_chunks < chunks_per_stream {
            let c_start = Instant::now();
            if should_cancel_midstream && received_chunks == 4 {
                adapter
                    .cancel_scope(cancel_scope, Generation::INITIAL)
                    .expect("cancel mid-stream scope");
                drop(rx);
                fenced_cancelled_streams += 1;
                break;
            }
            let Some(_chunk) = rx.recv().await else {
                break;
            };
            received_chunks += 1;
            chunk_latencies_ns
                .push(u64::try_from(c_start.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }

        // 1 amortized durable window checkpoint per non-cancelled stream (`WAL + FULL`).
        if !should_cancel_midstream {
            assert_eq!(received_chunks, chunks_per_stream);
            let head = channel_auth
                .inspect_channel(stream_channel.channel_id)
                .expect("inspect stream channel");
            let EnqueueDecision::Enqueued(rec) = channel_auth
                .enqueue(EnqueueRequest {
                    channel_id: stream_channel.channel_id,
                    expected_generation: head.generation,
                    expected_fencing_token: head.fencing_token,
                    payload: received_chunks.to_be_bytes().to_vec(),
                    idempotency_key: IdempotencyKey::from_bytes(id16(0x26, 0, s_idx)),
                    enqueued_at_ms: 2_000,
                })
                .expect("durable stream checkpoint enqueue")
            else {
                panic!("expected fresh stream checkpoint enqueue");
            };
            let _ = wait_auth
                .notify_commits(NotifyCommitsRequest {
                    channel_id: stream_channel.channel_id,
                    up_to_sequence: rec.sequence,
                    notified_at_ms: 2_001,
                    idempotency_key: IdempotencyKey::from_bytes(id16(0x27, 0, s_idx)),
                })
                .expect("notify stream commit");
        }

        let adapter_join = adapter.clone();
        let exit = tokio::task::spawn_blocking(move || adapter_join.join_fiber(producer_handle))
            .await
            .expect("join blocking")
            .expect("join producer fiber");
        if should_cancel_midstream {
            assert!(matches!(exit, FiberExit::Cancelled | FiberExit::Completed));
        } else {
            assert_eq!(exit, FiberExit::Completed);
        }
        backpressure_yields_observed += bp_flag.load(Ordering::Relaxed);
        stream_latencies_ns.push(u64::try_from(s_start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    let stream_wall = stream_wall_start.elapsed();
    let stream_stats = summarize_ns_as_us(&stream_latencies_ns);
    let chunk_stats = summarize_ns_as_us(&chunk_latencies_ns);
    assert_eq!(fenced_cancelled_streams, cancelled_streams_target);
    assert!(
        backpressure_yields_observed > 0,
        "bounded capacity=4 channel must engage producer backpressure across 16-chunk bursts"
    );

    // ------------------------------------------------------------------------
    // Class C: `Slow` — Durable Operation prepare/activate/complete + Outbox ack + Recovery
    // Durability barriers per op: 5 (`register + prepare + activate + complete + ack_outbox`)
    // ------------------------------------------------------------------------
    let slow_op_db = root.path().join("slow-operations.db");
    let op_store = Arc::new(SqliteOperationStore::open(&slow_op_db).expect("open slow op store"));
    assert!(verify_wal_full(&slow_op_db));
    let provider = MockProvider::new(Arc::clone(&op_store));

    let slow_ops = 48_u64;
    let mut slow_latencies_ns = Vec::with_capacity(usize::try_from(slow_ops).unwrap_or(0));
    let mut first_op_replay_probe = None;

    let slow_wall_start = Instant::now();
    for seq in 0..slow_ops {
        let op_start = Instant::now();
        let owner = FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(id16(0x30, 0, seq)),
            generation: Generation::INITIAL,
        };
        let reg = provider
            .register(OperationSpec {
                operation_id: OperationId::from_bytes(id16(0x31, 0, seq)),
                generation: Generation::INITIAL,
                owner_fiber: owner,
                cancellation_scope_id: CancellationScopeId::from_bytes(id16(0x32, 0, seq)),
                cancellation_generation: Generation::INITIAL,
            })
            .expect("slow provider register");
        let callback_id = CallbackId::from_bytes(id16(0x33, 0, seq));
        let disp = provider
            .dispatch(DispatchProviderOperation {
                handle: reg.handle,
                callback_id,
            })
            .expect("slow provider durable prepare+activate");
        assert!(!disp.replayed);

        let seed = digest32(0x34, 0, seq);
        let comp = provider
            .complete(CompleteProviderOperation {
                handle: reg.handle,
                callback_id,
                seed,
            })
            .expect("slow provider durable complete");
        assert!(!comp.replayed);
        assert!(comp.state.is_terminal());

        let pending = op_store.pending_outbox(8).expect("read pending outbox");
        assert_eq!(pending.len(), 1);
        op_store
            .acknowledge_outbox(pending[0].sequence)
            .expect("acknowledge outbox");

        if seq == 0 {
            first_op_replay_probe = Some((reg.handle, callback_id, seed, comp.state));
        }
        slow_latencies_ns.push(u64::try_from(op_start.elapsed().as_nanos()).unwrap_or(u64::MAX));
    }
    let slow_wall = slow_wall_start.elapsed();
    let slow_stats = summarize_ns_as_us(&slow_latencies_ns);

    // Verify restart recovery & idempotent terminal replay on reopened SqliteOperationStore!
    drop(provider);
    drop(op_store);
    let reopened_store =
        Arc::new(SqliteOperationStore::open(&slow_op_db).expect("reopen slow op store"));
    assert_eq!(
        reopened_store
            .pending_outbox(64)
            .expect("post-recovery outbox scan")
            .len(),
        0
    );
    let reopened_provider = MockProvider::new(Arc::clone(&reopened_store));
    let (probe_handle, probe_cb, probe_seed, expected_state) =
        first_op_replay_probe.expect("first op probe");
    let replay_comp = reopened_provider
        .complete(CompleteProviderOperation {
            handle: probe_handle,
            callback_id: probe_cb,
            seed: probe_seed,
        })
        .expect("idempotent replay after provider restart");
    assert!(replay_comp.replayed);
    assert_eq!(replay_comp.state, expected_state);
    drop(reopened_provider);
    drop(reopened_store);

    println!(
        "[c-bench] suite=PERF-CLASS-001 class=Fast ops={} barriers_per_op=0 throughput_ops_s={:.1} lat_us(p50={},p95={},p99={},max={})",
        fast_stats.count,
        ops_per_sec(fast_stats.count, fast_wall),
        fast_stats.p50_us,
        fast_stats.p95_us,
        fast_stats.p99_us,
        fast_stats.max_us,
    );
    println!(
        "[c-bench] suite=PERF-CLASS-001 class=Streaming streams={} chunks={} barriers_per_stream=1 barriers_per_chunk=0 bp_events={} cancel_fenced={}/{} stream_lat_us(p50={},p95={},p99={},max={}) chunk_lat_us(p50={},p95={},p99={},max={}) stream_ops_s={:.1}",
        stream_stats.count,
        chunk_stats.count,
        backpressure_yields_observed,
        fenced_cancelled_streams,
        cancelled_streams_target,
        stream_stats.p50_us,
        stream_stats.p95_us,
        stream_stats.p99_us,
        stream_stats.max_us,
        chunk_stats.p50_us,
        chunk_stats.p95_us,
        chunk_stats.p99_us,
        chunk_stats.max_us,
        ops_per_sec(stream_stats.count, stream_wall),
    );
    println!(
        "[c-bench] suite=PERF-CLASS-001 class=Slow ops={} barriers_per_op=5 throughput_ops_s={:.1} lat_us(p50={},p95={},p99={},max={})",
        slow_stats.count,
        ops_per_sec(slow_stats.count, slow_wall),
        slow_stats.p50_us,
        slow_stats.p95_us,
        slow_stats.p99_us,
        slow_stats.max_us,
    );

    assert!(
        fast_stats.p95_us < slow_stats.p95_us,
        "Fast path p95 ({} us) without sync disk barriers must beat durable Slow path p95 ({} us)",
        fast_stats.p95_us,
        slow_stats.p95_us
    );
}

// ============================================================================
// 2 & 3. [PERF-SCALE-001/002] + [SCALE-PROFILE-001] 8-Cell Sharded Scale Engine
// ============================================================================

struct ScaleAdmitAll;

impl AdmissionConsult for ScaleAdmitAll {
    type Error = Infallible;

    fn consult_materialization(
        &self,
        other_declared_task_nodes: u64,
    ) -> Result<AdmissionConsultOutcome, Infallible> {
        Ok(AdmissionConsultOutcome::Admitted(
            MaterializationAdmission {
                profile_id: "c-bench-sharded-scale".to_string(),
                projected_task_nodes: other_declared_task_nodes + 1,
                projected_active_working_set: 1,
            },
        ))
    }
}

fn mixed_dag_declaration(shard: u16, index: u64) -> PlanNodeDeclaration {
    let key_of = |i: u64| id16(0x60, shard, i);
    let mut dependency_keys = Vec::new();
    // ~11.7% zero-dependency roots (`index % 8 == 0` except nonzero multiples of 128),
    // ~88.3% DAG-dependent nodes (chains + periodic hub convergence onto node 0).
    if !index.is_multiple_of(8) {
        dependency_keys.push(key_of(index - 1));
    }
    if index > 0 && index.is_multiple_of(128) {
        dependency_keys.push(key_of(0));
    }
    PlanNodeDeclaration {
        node_key: key_of(index),
        kind: if index.is_multiple_of(2) {
            PlanNodeKind::AgentRole
        } else {
            PlanNodeKind::Executable
        },
        binding_digest: digest32(0x61, shard, index),
        dependency_keys,
        input_selectors_digest: digest32(0x62, shard, index),
        output_contract_digest: digest32(0x63, shard, index),
        policy_digest: digest32(0x64, shard, index),
        resource_ceiling_digest: digest32(0x65, shard, index),
        conditions: None,
    }
}

struct ScaleRehydratingHandler {
    shard: u16,
    index: u64,
    process: ProcessId,
    incarnation: Generation,
    handle: FiberHandle,
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: ChannelId,
    armed: Mutex<Vec<ChannelSequenceWait>>,
}

impl SnapshotResumable for ScaleRehydratingHandler {
    fn binding(&self) -> BindingId {
        BindingId::from_bytes(self.handle.fiber_id.into_bytes())
    }

    fn process_id(&self) -> ProcessId {
        self.process
    }

    fn expected_incarnation(&self) -> Generation {
        self.incarnation
    }

    fn handler_input(&self) -> Vec<u8> {
        digest32(0x71, self.shard, self.index).to_vec()
    }

    fn resume_from_entry(&self, input: &[u8]) -> Result<(), nlos_runtime_tokio::ResumeRejection> {
        if input != digest32(0x71, self.shard, self.index).as_slice() {
            return Err(nlos_runtime_tokio::ResumeRejection {
                reason: "handler_input mismatch during scale rehydrate".to_string(),
            });
        }
        let wait = self
            .adapter
            .wait_for_channel(
                self.handle,
                &self.waits,
                RegisterWaitRequest {
                    binding: self.binding(),
                    channel_id: self.channel_id,
                    target_sequence: 1,
                    idempotency_key: IdempotencyKey::from_bytes(id16(0x72, self.shard, self.index)),
                    registered_at_ms: 4_000,
                },
            )
            .map_err(|err| nlos_runtime_tokio::ResumeRejection {
                reason: format!("wait_for_channel failed: {err}"),
            })?;
        self.armed.lock().expect("lock armed").push(wait);
        Ok(())
    }
}

struct ShardScaleContext {
    shard_idx: u16,
    shard_dir: PathBuf,
    plan_db_path: PathBuf,
    plan_auth: SqlitePlanAuthority,
    plan_id: TaskPlanId,
    sample_node_ids: Vec<TaskNodeId>,
    mat_scheduler: MaterializationScheduler,
    cumulative_materialized: u64,
    task_auth: SqliteTaskAuthority,
    op_store: SqliteOperationStore,
    channel_auth: Arc<ChannelAuthority>,
    wait_auth: Arc<WaitAuthority>,
    process_auth: ProcessAuthority,
    channel_id: ChannelId,
    control_groups: Vec<TaskGroupId>,
    control_tasks: Vec<TaskId>,
}

#[allow(clippy::cast_precision_loss)]
async fn run_sharded_logical_scale_benchmark(
    tier_label: &str,
    total_logical_nodes: u64,
    working_set_ratios_pct: &[u64],
    ckpt_per_shard: u64,
    control_per_shard: u64,
) {
    assert_eq!(total_logical_nodes % (CLUSTER_CELL_SHARDS as u64), 0);
    let nodes_per_shard = total_logical_nodes / (CLUSTER_CELL_SHARDS as u64);
    let root = BenchRoot::new(tier_label);
    let ws_before = sample_process_working_set_bytes();

    // ------------------------------------------------------------------------
    // Stage 1 & 2: Sharded Logical `TaskNode` Registration + DAG Dependency Resolution
    //              across 8 independent Cell SQLite (`WAL + FULL`) Plan Authorities
    // ------------------------------------------------------------------------
    let reg_wall_start = Instant::now();
    let shard_init_results: Vec<(
        ShardScaleContext,
        u64, // apply_ns
        u64, // resolve_ns
        u64, // plan_db_bytes_after_declare
    )> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(CLUSTER_CELL_SHARDS);
        for s_idx in 0..CLUSTER_CELL_SHARDS {
            let shard_u16 = u16::try_from(s_idx).unwrap_or(0);
            let shard_dir = root.path().join(format!("cell-shard-{s_idx}"));
            handles.push(scope.spawn(move || {
                fs::create_dir_all(&shard_dir).expect("create shard dir");
                let plan_db_path = shard_dir.join("plan-authority.db");
                let plan_auth =
                    SqlitePlanAuthority::open(&plan_db_path).expect("open shard plan authority");
                assert!(verify_wal_full(&plan_db_path));

                let decls: Vec<PlanNodeDeclaration> = (0..nodes_per_shard)
                    .map(|i| mixed_dag_declaration(shard_u16, i))
                    .collect();
                let t_apply = Instant::now();
                let receipt = plan_auth
                    .apply_plan_revision_ungated(ApplyPlanRevisionRequest {
                        plan_id: None,
                        nodes: decls,
                        idempotency_key: IdempotencyKey::from_bytes(id16(0x66, shard_u16, 1)),
                        applied_at_ms: 1_000,
                    })
                    .expect("apply shard logical TaskNodes")
                    .receipt();
                let apply_ns = u64::try_from(t_apply.elapsed().as_nanos()).unwrap_or(u64::MAX);
                assert_eq!(receipt.declared_node_count, nodes_per_shard);
                let plan_db_bytes = sqlite_db_and_wal_bytes(&plan_db_path);

                let t_resolve = Instant::now();
                let res_handle = plan_auth
                    .resolve_plan(ResolvePlanRequest {
                        selector: PlanRevisionSelector::Current(receipt.plan_id),
                        idempotency_key: IdempotencyKey::from_bytes(id16(0x67, shard_u16, 1)),
                        resolved_at_ms: 1_500,
                    })
                    .expect("resolve shard DAG plan")
                    .handle();
                let resolve_ns = u64::try_from(t_resolve.elapsed().as_nanos()).unwrap_or(u64::MAX);
                let resolved_nodes = plan_auth
                    .inspect_resolved_nodes(res_handle.resolution_id)
                    .expect("inspect resolved nodes");
                assert_eq!(resolved_nodes.len() as u64, nodes_per_shard);
                let sample_node_ids: Vec<TaskNodeId> = resolved_nodes
                    .iter()
                    .take(usize::try_from(ckpt_per_shard.max(16)).unwrap_or(16))
                    .map(|n| n.node_id)
                    .collect();

                let task_db_path = shard_dir.join("task-authority.db");
                let task_auth =
                    SqliteTaskAuthority::open_with_scale_profile(&task_db_path, &TASK_PROFILE_100K)
                        .expect("open shard task authority");
                assert!(verify_wal_full(&task_db_path));

                let mut control_groups =
                    Vec::with_capacity(usize::try_from(control_per_shard).unwrap_or(0));
                let mut control_tasks =
                    Vec::with_capacity(usize::try_from(control_per_shard).unwrap_or(0));
                for c_idx in 0..control_per_shard {
                    let task_id = TaskId::from_bytes(id16(0x68, shard_u16, c_idx));
                    let group_id = TaskGroupId::from_bytes(id16(0x69, shard_u16, c_idx));
                    task_auth
                        .register_task(TaskSpec {
                            application_id: None,
                            plan_revision: None,
                            task_id,
                            task_generation: Generation::INITIAL,
                            registered_at_ms: 1_600,
                        })
                        .expect("seed shard task");
                    task_auth
                        .register_group(GroupSpec {
                            group_id,
                            task_id,
                            task_generation: Generation::INITIAL,
                            parent_group_id: None,
                            group_policy_digest: digest32(0x6A, shard_u16, c_idx),
                            completion_mode: CompletionMode::All,
                            failure_mode: FailureMode::FailFast,
                            max_children: 16,
                            max_depth: 3,
                            resource_group_id: None,
                            resource_account_digest: None,
                            cancellation_scope_id: CancellationScopeId::from_bytes(id16(
                                0x6B, shard_u16, c_idx,
                            )),
                            registered_at_ms: 1_601,
                        })
                        .expect("seed shard group");
                    control_tasks.push(task_id);
                    control_groups.push(group_id);
                }

                let op_db_path = shard_dir.join("op-authority.db");
                let op_store =
                    SqliteOperationStore::open(&op_db_path).expect("open shard op store");
                assert!(verify_wal_full(&op_db_path));

                let ch_dir = shard_dir.join("channel-wait-proc");
                fs::create_dir_all(&ch_dir).expect("create ch_dir");
                let channel_auth =
                    Arc::new(ChannelAuthority::open(&ch_dir).expect("open shard channel auth"));
                let wait_auth = Arc::new(
                    WaitAuthority::open(&ch_dir, Arc::clone(&channel_auth))
                        .expect("open shard wait auth"),
                );
                let process_auth =
                    ProcessAuthority::open(&ch_dir).expect("open shard process auth");
                let ChannelDecision::Created(ch_rec) = channel_auth
                    .create_channel(CreateChannelRequest {
                        capacity_bytes: 65_536,
                        policy_digest: [0x6C; 32],
                        idempotency_key: IdempotencyKey::from_bytes(id16(0x6D, shard_u16, 1)),
                        created_at_ms: 1_700,
                    })
                    .expect("create shard channel")
                else {
                    panic!("expected fresh shard channel");
                };

                let ctx = ShardScaleContext {
                    shard_idx: shard_u16,
                    shard_dir,
                    plan_db_path,
                    plan_auth,
                    plan_id: receipt.plan_id,
                    sample_node_ids,
                    mat_scheduler: MaterializationScheduler::new(0),
                    cumulative_materialized: 0,
                    task_auth,
                    op_store,
                    channel_auth,
                    wait_auth,
                    process_auth,
                    channel_id: ch_rec.channel_id,
                    control_groups,
                    control_tasks,
                };
                (ctx, apply_ns, resolve_ns, plan_db_bytes)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("join shard init"))
            .collect()
    });
    let reg_wall_elapsed = reg_wall_start.elapsed();

    let mut shards = Vec::with_capacity(CLUSTER_CELL_SHARDS);
    let mut apply_samples_ns = Vec::with_capacity(CLUSTER_CELL_SHARDS);
    let mut resolve_samples_ns = Vec::with_capacity(CLUSTER_CELL_SHARDS);
    let mut total_plan_declare_bytes = 0_u64;
    for (ctx, apply_ns, resolve_ns, plan_bytes) in shard_init_results {
        shards.push(ctx);
        apply_samples_ns.push(apply_ns);
        resolve_samples_ns.push(resolve_ns);
        total_plan_declare_bytes += plan_bytes;
    }
    let bytes_per_logical_node = total_plan_declare_bytes / total_logical_nodes;
    let ws_after_declare = sample_process_working_set_bytes();
    let apply_stats = summarize_ns_as_us(&apply_samples_ns);
    let resolve_stats = summarize_ns_as_us(&resolve_samples_ns);

    println!(
        "[c-bench] tier={} stage=logical_registration_and_dag_eligibility shards={} logical_nodes={} nodes_per_shard={} wall_ms={:.2} reg_nodes_per_s={:.1} shard_apply_us(p50={},p95={},max={}) shard_resolve_us(p50={},p95={},max={}) plan_declare_bytes={} bytes_per_logical_node={} ws_before_mb={:.2} ws_after_declare_mb={:.2}",
        tier_label,
        CLUSTER_CELL_SHARDS,
        total_logical_nodes,
        nodes_per_shard,
        reg_wall_elapsed.as_secs_f64() * 1_000.0,
        ops_per_sec(
            usize::try_from(total_logical_nodes).unwrap_or(0),
            reg_wall_elapsed
        ),
        apply_stats.p50_us,
        apply_stats.p95_us,
        apply_stats.max_us,
        resolve_stats.p50_us,
        resolve_stats.p95_us,
        resolve_stats.max_us,
        total_plan_declare_bytes,
        bytes_per_logical_node,
        ws_before.unwrap_or(0) as f64 / (1024.0 * 1024.0),
        ws_after_declare.unwrap_or(0) as f64 / (1024.0 * 1024.0),
    );
    assert!(
        bytes_per_logical_node <= 4_096,
        "durable metadata per logical TaskNode ({bytes_per_logical_node} B) must stay bounded <= 4096 B"
    );

    // ------------------------------------------------------------------------
    // Stage 3: Active Working-Set Ratio Sweep (`1%`, `5%`, `10%` — `[SCALE-PROFILE-001]`)
    //          Materialization -> Runnable Queue -> ThreeTierScheduler Dispatch & Stealing
    //          -> M:N ExecutionFiber Execution on 8 Tokio Worker Threads
    // ------------------------------------------------------------------------
    let max_active = usize::try_from((total_logical_nodes * 10) / 100).unwrap_or(1_024);
    let scheduler = ThreeTierScheduler::new(GLOBAL_DOMAIN_BYTES, max_active * 2);
    let mut cell_domains = Vec::with_capacity(CLUSTER_CELL_SHARDS);
    let mut cell_worker_pairs = Vec::with_capacity(CLUSTER_CELL_SHARDS);

    for s_idx in 0..CLUSTER_CELL_SHARDS {
        let per_cell_cap = (max_active / CLUSTER_CELL_SHARDS).max(64) * 4;
        let cell_dom = scheduler.add_cell(cell_nominal_id(s_idx), per_cell_cap);
        cell_dom.register_resource_group(ResourceGroupQueuePolicy {
            resource_group_id: RG_PRIMARY,
            max_queue_depth: per_cell_cap,
            parent_hard_quota_in_flight: per_cell_cap,
            weight: 4,
            max_priority_cap: 220,
            starvation_aging_step_ms: 50,
        });
        let w0 = cell_dom.add_worker(
            worker_nominal_id(s_idx, 0),
            WorkerDomainConfig {
                max_concurrency: per_cell_cap,
                base_materialization_window: per_cell_cap,
            },
            default_worker_capabilities(),
        );
        let w1 = cell_dom.add_worker(
            worker_nominal_id(s_idx, 1),
            WorkerDomainConfig {
                max_concurrency: per_cell_cap,
                base_materialization_window: per_cell_cap,
            },
            default_worker_capabilities(),
        );
        cell_domains.push(cell_dom);
        cell_worker_pairs.push((w0, w1));
    }

    let fiber_batch_cap = 2_048_usize;
    let runtime_adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: fiber_batch_cap + 64,
            tombstone_capacity: 512,
            scope_tombstone_capacity: 512,
            orphan_buffer_capacity: 64,
        },
    )
    .expect("create scale TokioRuntimeAdapter");

    for &ratio_pct in working_set_ratios_pct {
        let target_total_active = (total_logical_nodes * ratio_pct) / 100;
        let base_per_shard = target_total_active / (CLUSTER_CELL_SHARDS as u64);
        let rem_shards = target_total_active % (CLUSTER_CELL_SHARDS as u64);

        // 3a. Durable Materialization across all 8 parallel Cell shards (`select` + `drive`).
        let mat_start = Instant::now();
        let mat_delta_results: Vec<(u64, u64, u64)> = thread::scope(|scope| {
            let mut handles = Vec::new();
            for (s_idx, shard) in shards.iter_mut().enumerate() {
                let shard_target = base_per_shard + u64::from((s_idx as u64) < rem_shards);
                handles.push(scope.spawn(move || {
                    let prev = shard.cumulative_materialized;
                    let delta_expected = shard_target.saturating_sub(prev);
                    shard.mat_scheduler.set_window(shard_target);
                    let t_sel = Instant::now();
                    let report = shard
                        .mat_scheduler
                        .select(&shard.plan_auth, shard.plan_id)
                        .expect("select ready logical nodes");
                    let sel_ns = u64::try_from(t_sel.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    assert_eq!(report.selections.len() as u64, delta_expected);

                    let t_drv = Instant::now();
                    let summary = shard
                        .mat_scheduler
                        .drive(&shard.plan_auth, &report, &ScaleAdmitAll, 2_000)
                        .expect("drive durable materialization");
                    let drv_ns = u64::try_from(t_drv.elapsed().as_nanos()).unwrap_or(u64::MAX);
                    assert_eq!(summary.approved, delta_expected);
                    shard.cumulative_materialized = shard_target;
                    (delta_expected, sel_ns, drv_ns)
                }));
            }
            handles
                .into_iter()
                .map(|h| h.join().expect("join mat"))
                .collect()
        });
        let mat_wall = mat_start.elapsed();
        let newly_materialized: u64 = mat_delta_results.iter().map(|r| r.0).sum();
        let total_materialized_now = target_total_active;
        let unmaterialized_metadata_only = total_logical_nodes - total_materialized_now;

        // 3b. Enqueue runnable working set into ThreeTierScheduler (skewed onto w0 of each Cell
        //     first so w1 exercises real intra-Cell work stealing!).
        let mut sched_dispatch_ns =
            Vec::with_capacity(usize::try_from(target_total_active).unwrap_or(0));
        let mut stolen_items = 0_u64;
        let sched_start = Instant::now();
        for (s_idx, (w0, _w1)) in cell_worker_pairs.iter().enumerate() {
            let shard_u16 = u16::try_from(s_idx).unwrap_or(0);
            let shard_target = base_per_shard + u64::from((s_idx as u64) < rem_shards);
            for local_i in 0..shard_target {
                let t0 = Instant::now();
                let item = make_work_item(
                    0x80 + u8::try_from(ratio_pct).unwrap_or(0),
                    shard_u16,
                    local_i,
                    RG_PRIMARY,
                    100,
                    DeadlineClass::Interactive,
                    local_i,
                );
                let adm = w0.enqueue(item, local_i);
                assert!(adm.is_admitted());
                sched_dispatch_ns.push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
        }
        let runnable_queued_peak = target_total_active;

        // Idle peer worker `w1` steals up to 25% of each Cell's runnable queue from `w0`!
        let steals_per_cell = (base_per_shard / 4).max(1);
        for (w0, w1) in &cell_worker_pairs {
            for tick in 0..steals_per_cell {
                let steal_rep = w1
                    .steal_from_peer_worker(w0, 1_000 + tick)
                    .expect("intra-Cell work steal");
                if steal_rep.stolen_item_id.is_some() {
                    stolen_items += 1;
                }
            }
        }

        // Dequeue all runnable items across the 16 WorkerSchedulerDomains and execute them as
        // M:N ExecutionFibers on the 8-thread TokioRuntimeAdapter!
        let mut dequeued_total = 0_u64;
        for (w0, w1) in &cell_worker_pairs {
            while let Some(d) = w0.dequeue_next(2_000) {
                w0.complete_in_flight(d.item.resource_group_id);
                dequeued_total += 1;
            }
            while let Some(d) = w1.dequeue_next(2_000) {
                w1.complete_in_flight(d.item.resource_group_id);
                dequeued_total += 1;
            }
        }
        assert_eq!(dequeued_total, target_total_active);
        let sched_wall = sched_start.elapsed();

        // Execute M:N fibers in bounded batches (`<= fiber_batch_cap`) on the 8-host-thread pool!
        let total_active_usize = usize::try_from(target_total_active).unwrap_or(0);
        let mut fiber_latencies_ns = Vec::with_capacity(total_active_usize);
        let mut active_fibers_peak = 0_usize;
        let fiber_wall_start = Instant::now();
        let mut offset = 0_usize;
        while offset < total_active_usize {
            let batch_len = (total_active_usize - offset).min(fiber_batch_cap);
            let mut batch_handles = Vec::with_capacity(batch_len);
            for b_i in 0..batch_len {
                let global_seq = u64::try_from(offset + b_i).unwrap_or(0);
                let shard_u16 = u16::try_from((offset + b_i) % CLUSTER_CELL_SHARDS).unwrap_or(0);
                let t_sp = Instant::now();
                let spec = make_fiber_spec(
                    0x90 + u8::try_from(ratio_pct).unwrap_or(0),
                    shard_u16,
                    global_seq,
                    Generation::INITIAL,
                );
                let h = runtime_adapter
                    .spawn_fiber(
                        spec,
                        Box::pin(async move {
                            tokio::task::yield_now().await;
                            FiberExit::Completed
                        }),
                    )
                    .expect("spawn M:N scale fiber");
                batch_handles.push((h, t_sp));
            }
            active_fibers_peak = active_fibers_peak.max(runtime_adapter.registered_fibers());
            let adapter_join = runtime_adapter.clone();
            let completed_ns = tokio::task::spawn_blocking(move || {
                let mut lats = Vec::with_capacity(batch_handles.len());
                for (h, t_sp) in batch_handles {
                    let exit = adapter_join.join_fiber(h).expect("join M:N fiber");
                    assert_eq!(exit, FiberExit::Completed);
                    lats.push(u64::try_from(t_sp.elapsed().as_nanos()).unwrap_or(u64::MAX));
                }
                lats
            })
            .await
            .expect("join M:N batch");
            fiber_latencies_ns.extend(completed_ns);
            offset += batch_len;
        }
        let fiber_wall = fiber_wall_start.elapsed();
        assert_eq!(runtime_adapter.registered_fibers(), 0);

        let sched_stats = summarize_ns_as_us(&sched_dispatch_ns);
        let fiber_stats = summarize_ns_as_us(&fiber_latencies_ns);
        let ws_ratio_bytes = sample_process_working_set_bytes();

        // Durable transition throughput for this ratio step: each newly materialized node commits
        // 2 durable state transitions (`request` + `resolve`), plus scheduler & fiber transitions.
        let state_transitions =
            usize::try_from(newly_materialized * 2 + target_total_active * 2).unwrap_or(0);
        let combined_wall = mat_wall + sched_wall + fiber_wall;

        println!(
            "[c-bench] tier={} stage=working_set_profile ratio_pct={}% logical={} unmaterialized_metadata_only={} materialized={} runnable_peak={} active_fibers_peak={} fiber_dispatched={} host_threads={} stolen_items={} mat_delta={} mat_wall_ms={:.2} sched_wall_ms={:.2} fiber_wall_ms={:.2} state_transitions_per_s={:.1} sched_enqueue_us(p50={},p95={},p99={}) fiber_e2e_us(p50={},p95={},p99={}) process_ws_mb={:.2}",
            tier_label,
            ratio_pct,
            total_logical_nodes,
            unmaterialized_metadata_only,
            total_materialized_now,
            runnable_queued_peak,
            active_fibers_peak,
            total_active_usize,
            HOST_RUNTIME_THREADS,
            stolen_items,
            newly_materialized,
            mat_wall.as_secs_f64() * 1_000.0,
            sched_wall.as_secs_f64() * 1_000.0,
            fiber_wall.as_secs_f64() * 1_000.0,
            ops_per_sec(state_transitions, combined_wall),
            sched_stats.p50_us,
            sched_stats.p95_us,
            sched_stats.p99_us,
            fiber_stats.p50_us,
            fiber_stats.p95_us,
            fiber_stats.p99_us,
            ws_ratio_bytes.unwrap_or(0) as f64 / (1024.0 * 1024.0),
        );
    }

    // ------------------------------------------------------------------------
    // Stage 4 & 5: Sharded Durable Checkpoint / Evict / Rehydrate + Residency Walk
    //              + Channel / Wait Delivery across all 8 Cell shards
    // ------------------------------------------------------------------------
    let ckpt_total_nodes =
        usize::try_from(ckpt_per_shard * (CLUSTER_CELL_SHARDS as u64)).unwrap_or(0);
    let mut ckpt_latencies_ns = Vec::with_capacity(ckpt_total_nodes);
    let mut rehydrate_latencies_ns = Vec::with_capacity(ckpt_total_nodes);
    let mut channel_wake_latencies_ns = Vec::with_capacity(ckpt_total_nodes);

    let ckpt_adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: ckpt_total_nodes + 32,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("ckpt adapter");

    let mut evicted_meta = Vec::with_capacity(ckpt_total_nodes);
    for shard in &shards {
        for idx in 0..ckpt_per_shard {
            let t_ck = Instant::now();
            let node_id = shard.sample_node_ids[usize::try_from(idx).unwrap_or(0)];
            // Promote durable residency MetadataOnly -> Cold -> Warm -> Hot before checkpoint.
            for (from_tier, to_tier, tag) in [
                (
                    NodeResidencyTier::MetadataOnly,
                    NodeResidencyTier::Cold,
                    0xA1_u8,
                ),
                (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0xA2),
                (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0xA3),
            ] {
                shard
                    .plan_auth
                    .record_residency_transition(ResidencyTransitionRequest {
                        plan_id: shard.plan_id,
                        node_id,
                        from_tier,
                        to_tier,
                        expected_declared_revision: 1,
                        idempotency_key: IdempotencyKey::from_bytes(id16(
                            tag,
                            shard.shard_idx,
                            idx,
                        )),
                        transitioned_at_ms: 3_000,
                    })
                    .expect("promote residency to Hot");
            }

            let IsolationDomainDecision::Created(iso_dom) = shard
                .process_auth
                .create_isolation_domain(CreateIsolationDomainRequest {
                    policy_digest: [0x11; 32],
                    idempotency_key: IdempotencyKey::from_bytes(id16(0xA4, shard.shard_idx, idx)),
                    created_at_ms: 3_010,
                })
                .expect("create isolation domain")
            else {
                panic!("expected fresh isolation domain");
            };
            let ProcessBindingDecision::Registered(proc_bind) = shard
                .process_auth
                .register_delegated_process(RegisterDelegatedProcessRequest {
                    task_id: TaskId::from_bytes(id16(0xA5, shard.shard_idx, idx)),
                    task_attempt_id: TaskAttemptId::from_bytes(id16(0xA6, shard.shard_idx, idx)),
                    attempt_generation: Generation::INITIAL,
                    isolation_domain_id: IsolationDomainId::from_bytes(
                        iso_dom.isolation_domain_id.into_bytes(),
                    ),
                    isolation_domain_generation: iso_dom.generation,
                    isolation_domain_fencing_token: iso_dom.fencing_token,
                    idempotency_key: IdempotencyKey::from_bytes(id16(0xA7, shard.shard_idx, idx)),
                    created_at_ms: 3_020,
                })
                .expect("register delegated process")
            else {
                panic!("expected fresh process binding");
            };
            let mut spec = make_fiber_spec(0xA8, shard.shard_idx, idx, Generation::INITIAL);
            spec.process_id = proc_bind.process_id;
            spec.process_generation = proc_bind.process_generation;
            let FiberIncarnationDecision::Registered(inc) = shard
                .process_auth
                .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                    process_id: proc_bind.process_id,
                    expected_process_generation: proc_bind.process_generation,
                    expected_process_fencing_token: proc_bind.process_fencing_token,
                    binding: spec.fiber_id,
                    idempotency_key: IdempotencyKey::from_bytes(id16(0xA9, shard.shard_idx, idx)),
                    registered_at_ms: 3_030,
                })
                .expect("register fiber incarnation")
            else {
                panic!("expected fresh incarnation");
            };
            let handle = ckpt_adapter
                .spawn_fiber(spec, Box::pin(pending()))
                .expect("spawn checkpointed fiber");
            let handler = ScaleRehydratingHandler {
                shard: shard.shard_idx,
                index: idx,
                process: proc_bind.process_id,
                incarnation: inc.incarnation_generation,
                handle,
                adapter: ckpt_adapter.clone(),
                waits: Arc::clone(&shard.wait_auth),
                channel_id: shard.channel_id,
                armed: Mutex::new(Vec::new()),
            };
            let snap = ckpt_adapter
                .snapshot_handler_entry(handle, &shard.process_auth, &handler)
                .expect("snapshot_handler_entry")
                .expect("snapshot written");
            assert_eq!(snap.handler_input.len(), 32);

            // Demote Hot -> Warm -> Cold upon eviction!
            for (from_tier, to_tier, tag) in [
                (NodeResidencyTier::Hot, NodeResidencyTier::Warm, 0xAA_u8),
                (NodeResidencyTier::Warm, NodeResidencyTier::Cold, 0xAB),
            ] {
                shard
                    .plan_auth
                    .record_residency_transition(ResidencyTransitionRequest {
                        plan_id: shard.plan_id,
                        node_id,
                        from_tier,
                        to_tier,
                        expected_declared_revision: 1,
                        idempotency_key: IdempotencyKey::from_bytes(id16(
                            tag,
                            shard.shard_idx,
                            idx,
                        )),
                        transitioned_at_ms: 3_040,
                    })
                    .expect("demote residency to Cold on evict");
            }
            ckpt_latencies_ns.push(u64::try_from(t_ck.elapsed().as_nanos()).unwrap_or(u64::MAX));
            evicted_meta.push((
                usize::from(shard.shard_idx),
                idx,
                node_id,
                proc_bind.process_id,
                proc_bind.process_fencing_token,
                spec.fiber_id,
            ));
        }
    }

    // Evict entire checkpoint runtime!
    drop(ckpt_adapter);

    let rehydrate_adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: ckpt_total_nodes + 32,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("rehydrate adapter");

    let mut rehydrated_waiters = Vec::with_capacity(ckpt_total_nodes);
    for &(shard_pos, idx, node_id, proc_id, proc_fence, fiber_id) in &evicted_meta {
        let shard = &shards[shard_pos];
        let t_re = Instant::now();
        // Promote Cold -> Warm -> Hot on rehydrate!
        for (from_tier, to_tier, tag) in [
            (NodeResidencyTier::Cold, NodeResidencyTier::Warm, 0xAC_u8),
            (NodeResidencyTier::Warm, NodeResidencyTier::Hot, 0xAD),
        ] {
            shard
                .plan_auth
                .record_residency_transition(ResidencyTransitionRequest {
                    plan_id: shard.plan_id,
                    node_id,
                    from_tier,
                    to_tier,
                    expected_declared_revision: 1,
                    idempotency_key: IdempotencyKey::from_bytes(id16(tag, shard.shard_idx, idx)),
                    transitioned_at_ms: 3_500,
                })
                .expect("promote residency back to Hot on rehydrate");
        }
        let FiberIncarnationDecision::Registered(next_inc) = shard
            .process_auth
            .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                process_id: proc_id,
                expected_process_generation: Generation::INITIAL,
                expected_process_fencing_token: proc_fence,
                binding: fiber_id,
                idempotency_key: IdempotencyKey::from_bytes(id16(0xAE, shard.shard_idx, idx)),
                registered_at_ms: 3_510,
            })
            .expect("register next incarnation")
        else {
            panic!("expected next incarnation");
        };
        let next_gen = next_inc.incarnation_generation;
        let mut spec = make_fiber_spec(0xA8, shard.shard_idx, idx, next_gen);
        spec.process_id = proc_id;
        spec.process_generation = Generation::INITIAL;
        let handle = rehydrate_adapter
            .spawn_fiber(spec, Box::pin(pending()))
            .expect("spawn rehydrated fiber");
        let handler = ScaleRehydratingHandler {
            shard: shard.shard_idx,
            index: idx,
            process: proc_id,
            incarnation: next_inc.incarnation_generation,
            handle,
            adapter: rehydrate_adapter.clone(),
            waits: Arc::clone(&shard.wait_auth),
            channel_id: shard.channel_id,
            armed: Mutex::new(Vec::new()),
        };
        let restored = rehydrate_adapter
            .resume_from_snapshot(handle, &shard.process_auth, &handler)
            .expect("resume_from_snapshot")
            .restored
            .expect("restored snapshot");
        assert_eq!(restored.handler_input.len(), 32);
        assert_eq!(
            rehydrate_adapter
                .inspect(handle)
                .expect("inspect rehydrated"),
            FiberState::WaitingIo
        );
        let wait_fut = handler
            .armed
            .lock()
            .expect("lock armed")
            .pop()
            .expect("armed wait future");
        rehydrate_latencies_ns.push(u64::try_from(t_re.elapsed().as_nanos()).unwrap_or(u64::MAX));
        rehydrated_waiters.push((shard_pos, idx, handle, spec.cancellation_scope_id, wait_fut));
    }

    // Stage 5: Channel / Topic Delivery waking all rehydrated fibers across the 8 Cell shards!
    let sink = rehydrate_adapter.channel_wait_sink();
    for (s_idx, shard) in shards.iter().enumerate() {
        let shard_u16 = u16::try_from(s_idx).unwrap_or(0);
        let t_ch = Instant::now();
        let head = shard
            .channel_auth
            .inspect_channel(shard.channel_id)
            .expect("inspect shard channel");
        let EnqueueDecision::Enqueued(enq) = shard
            .channel_auth
            .enqueue(EnqueueRequest {
                channel_id: shard.channel_id,
                expected_generation: head.generation,
                expected_fencing_token: head.fencing_token,
                payload: vec![0x7E; 64],
                idempotency_key: IdempotencyKey::from_bytes(id16(0xAF, shard_u16, 1)),
                enqueued_at_ms: 4_100,
            })
            .expect("enqueue shard wake message")
        else {
            panic!("expected fresh shard channel enqueue");
        };
        let wake_report = shard
            .wait_auth
            .notify_commits(NotifyCommitsRequest {
                channel_id: shard.channel_id,
                up_to_sequence: enq.sequence,
                notified_at_ms: 4_110,
                idempotency_key: IdempotencyKey::from_bytes(id16(0xB1, shard_u16, 1)),
            })
            .expect("notify shard channel commits");
        assert_eq!(wake_report.woken.len() as u64, ckpt_per_shard);
        let delivery = sink.deliver(&wake_report).expect("deliver shard wakes");
        assert_eq!(
            delivery,
            DeliveryReport {
                delivered: usize::try_from(ckpt_per_shard).unwrap_or(0),
                buffered: 0,
            }
        );
        let per_wake_ns =
            u64::try_from(t_ch.elapsed().as_nanos()).unwrap_or(u64::MAX) / ckpt_per_shard.max(1);
        for _ in 0..ckpt_per_shard {
            channel_wake_latencies_ns.push(per_wake_ns);
        }
    }

    for (shard_pos, _idx, handle, scope_id, wait_fut) in rehydrated_waiters {
        let outcome = tokio::time::timeout(Duration::from_secs(5), wait_fut)
            .await
            .expect("rehydrated channel wait resolves");
        assert_eq!(outcome, WaitOutcome::Woken);
        let rows = shards[shard_pos]
            .wait_auth
            .list_waits_for_binding(BindingId::from_bytes(handle.fiber_id.into_bytes()))
            .expect("list binding waits");
        assert_eq!(rows[0].state, WaitState::Woken);
        rehydrate_adapter
            .cancel_scope(scope_id, Generation::INITIAL)
            .expect("cleanup rehydrated scope");
        let adapter_join = rehydrate_adapter.clone();
        let _ = tokio::task::spawn_blocking(move || adapter_join.join_fiber(handle))
            .await
            .expect("join cleanup");
    }
    drop(rehydrate_adapter);

    let ckpt_stats = summarize_ns_as_us(&ckpt_latencies_ns);
    let rehydrate_stats = summarize_ns_as_us(&rehydrate_latencies_ns);
    let wake_stats = summarize_ns_as_us(&channel_wake_latencies_ns);

    println!(
        "[c-bench] tier={} stage=checkpoint_evict_rehydrate_and_channel_wake nodes={} ckpt_us(p50={},p95={},p99={},max={}) rehydrate_us(p50={},p95={},p99={},max={}) channel_wake_us(p50={},p95={},p99={},max={})",
        tier_label,
        ckpt_total_nodes,
        ckpt_stats.p50_us,
        ckpt_stats.p95_us,
        ckpt_stats.p99_us,
        ckpt_stats.max_us,
        rehydrate_stats.p50_us,
        rehydrate_stats.p95_us,
        rehydrate_stats.p99_us,
        rehydrate_stats.max_us,
        wake_stats.p50_us,
        wake_stats.p95_us,
        wake_stats.p99_us,
        wake_stats.max_us,
    );

    // ------------------------------------------------------------------------
    // Stage 6: [PERF-CONTROL-001] Manual Control Responsiveness under 10K/100K Load
    //          (key-scoped point queries & group/task cancels — zero full-agent table scan!)
    // ------------------------------------------------------------------------
    let mut control_latencies_ns = Vec::new();
    for shard in &shards {
        for c_idx in 0..control_per_shard {
            let idx_usize = usize::try_from(c_idx).unwrap_or(0);
            let t_ctrl = Instant::now();
            match c_idx % 5 {
                0 => {
                    let node_rec = shard
                        .plan_auth
                        .inspect_node(
                            shard.plan_id,
                            shard.sample_node_ids[idx_usize % shard.sample_node_ids.len()],
                        )
                        .expect("key-scoped inspect_node")
                        .expect("node exists");
                    assert_eq!(node_rec.plan_id, shard.plan_id);
                }
                1 => {
                    let res_view = shard
                        .plan_auth
                        .inspect_node_residency(
                            shard.plan_id,
                            shard.sample_node_ids[idx_usize % shard.sample_node_ids.len()],
                        )
                        .expect("key-scoped inspect_node_residency")
                        .expect("node residency exists");
                    assert_eq!(res_view.plan_id, shard.plan_id);
                }
                2 => {
                    let dec = shard
                        .task_auth
                        .cancel_group(GroupCancelRequest {
                            group_id: shard.control_groups[idx_usize],
                            idempotency_key: IdempotencyKey::from_bytes(id16(
                                0xB2,
                                shard.shard_idx,
                                c_idx,
                            )),
                            requested_at_ms: 4_500,
                        })
                        .expect("cancel_group under 10K/100K load");
                    assert!(matches!(dec, GroupCancelDecision::Applied { .. }));
                }
                3 => {
                    let dec = shard
                        .task_auth
                        .cancel_task(CancelRequest {
                            task_id: shard.control_tasks[idx_usize],
                            idempotency_key: IdempotencyKey::from_bytes(id16(
                                0xB3,
                                shard.shard_idx,
                                c_idx,
                            )),
                            requested_at_ms: 4_500,
                        })
                        .expect("cancel_task under 10K/100K load");
                    assert!(matches!(dec, CancelDecision::Applied { .. }));
                }
                _ => {
                    let snap = shard
                        .task_auth
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
                    let _ = shard
                        .task_auth
                        .drive_working_set_reclaim(
                            WorkingSetReclaimExecutionRequest {
                                execution,
                                executed_at_ms: 4_500,
                            },
                            &UnlinkedReclaimResidency,
                        )
                        .expect("drive_working_set_reclaim");
                    assert_eq!(
                        shard
                            .op_store
                            .pending_outbox(8)
                            .expect("bounded op outbox probe")
                            .len(),
                        0
                    );
                }
            }
            control_latencies_ns
                .push(u64::try_from(t_ctrl.elapsed().as_nanos()).unwrap_or(u64::MAX));
        }
    }
    let ctrl_stats = summarize_ns_as_us(&control_latencies_ns);
    assert!(
        ctrl_stats.p50_us < 25_000 && ctrl_stats.p95_us < 100_000 && ctrl_stats.p99_us < 500_000,
        "{tier_label} [PERF-CONTROL-001] manual control latency ({ctrl_stats:?}) must stay interactive",
    );

    // ------------------------------------------------------------------------
    // Stage 7: Bounded Recovery Scan + Total Durable Disk Footprint across 8 Cell shards
    // ------------------------------------------------------------------------
    let shard_dirs_and_plans: Vec<(PathBuf, PathBuf, TaskPlanId)> = shards
        .iter()
        .map(|s| (s.shard_dir.clone(), s.plan_db_path.clone(), s.plan_id))
        .collect();
    // Explicitly close all SQLite connections before measuring post-close footprint & reopen scan!
    drop(shards);

    let total_cluster_disk_bytes = directory_durable_bytes_recursive(root.path());
    let total_plan_final_bytes: u64 = shard_dirs_and_plans
        .iter()
        .map(|(_, plan_db, _)| sqlite_db_and_wal_bytes(plan_db))
        .sum();

    let recovery_wall_start = Instant::now();
    let shard_recovery_ns: Vec<u64> = thread::scope(|scope| {
        let mut handles = Vec::with_capacity(CLUSTER_CELL_SHARDS);
        for (shard_dir, plan_db, plan_id) in &shard_dirs_and_plans {
            handles.push(scope.spawn(move || {
                let t0 = Instant::now();
                let plan_reopen =
                    SqlitePlanAuthority::open(plan_db).expect("reopen shard plan authority");
                let plan_view = plan_reopen
                    .inspect_plan(*plan_id)
                    .expect("bounded plan head recovery inspect")
                    .expect("plan exists on reopen");
                assert_eq!(plan_view.plan_id, *plan_id);
                assert_eq!(plan_view.current_revision, 1);

                let task_reopen = SqliteTaskAuthority::open_with_scale_profile(
                    shard_dir.join("task-authority.db"),
                    &TASK_PROFILE_100K,
                )
                .expect("reopen shard task authority");
                let _pressure = task_reopen
                    .inspect_working_set_pressure()
                    .expect("bounded task working-set recovery inspect");

                let op_reopen = SqliteOperationStore::open(shard_dir.join("op-authority.db"))
                    .expect("reopen shard op store");
                let pending = op_reopen
                    .pending_outbox(128)
                    .expect("bounded outbox recovery scan");
                assert_eq!(pending.len(), 0);

                u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX)
            }));
        }
        handles
            .into_iter()
            .map(|h| h.join().expect("join shard recovery"))
            .collect()
    });
    let recovery_wall = recovery_wall_start.elapsed();
    let recovery_stats = summarize_ns_as_us(&shard_recovery_ns);
    let ws_final = sample_process_working_set_bytes();

    println!(
        "[c-bench] tier={} stage=control_and_recovery_and_footprint ctrl_ops={} ctrl_us(p50={},p95={},p99={},max={}) recovery_wall_ms={:.2} shard_recovery_us(p50={},p95={},max={}) plan_final_bytes={} total_cluster_disk_bytes={} final_ws_mb={:.2}",
        tier_label,
        ctrl_stats.count,
        ctrl_stats.p50_us,
        ctrl_stats.p95_us,
        ctrl_stats.p99_us,
        ctrl_stats.max_us,
        recovery_wall.as_secs_f64() * 1_000.0,
        recovery_stats.p50_us,
        recovery_stats.p95_us,
        recovery_stats.max_us,
        total_plan_final_bytes,
        total_cluster_disk_bytes,
        ws_final.unwrap_or(0) as f64 / (1024.0 * 1024.0),
    );
    assert!(
        recovery_wall < Duration::from_secs(10),
        "{tier_label} 8-shard bounded recovery scan ({recovery_wall:?}) exceeded 10s ceiling"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn perf_scale_001_002_sharded_10k_logical_multi_working_set_benchmark() {
    run_sharded_logical_scale_benchmark("10K_LOGICAL", 10_000, &[1, 5, 10], 8, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "scale probe: 100K_LOGICAL 8-Cell sharded durable benchmark — run with --release -- --include-ignored --nocapture"]
async fn perf_scale_001_002_sharded_100k_logical_multi_working_set_benchmark() {
    run_sharded_logical_scale_benchmark("100K_LOGICAL", 100_000, &[1, 5, 10], 16, 16).await;
}

// ============================================================================
// 4. [ROAD-C-002] Fault Scenario 4: Provider Stall under Multi-Cell Load
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn road_c002_fault_provider_stall_isolates_bulkhead_fences_timeout_and_preserves_fast_path() {
    let root = BenchRoot::new("fault-provider-stall");
    let adapter = TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers: 256,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("create runtime adapter");

    let op_db_stalled = root.path().join("provider-stalled-ops.db");
    let op_db_healthy = root.path().join("provider-healthy-ops.db");
    let stalled_store =
        Arc::new(SqliteOperationStore::open(&op_db_stalled).expect("open stalled op store"));
    let healthy_store =
        Arc::new(SqliteOperationStore::open(&op_db_healthy).expect("open healthy op store"));
    let stalled_provider = Arc::new(MockProvider::new(Arc::clone(&stalled_store)));
    let healthy_provider = Arc::new(MockProvider::new(Arc::clone(&healthy_store)));

    let tenant_id = FanoutTenantId::from_bytes(id16(0xC1, 0, 1));
    let workspace_id = WorkspaceId::from_bytes(id16(0xC2, 0, 1));
    let group_id = TaskGroupId::from_bytes(id16(0xC3, 0, 1));
    let prov_stalled_id = FanoutProviderId::from_bytes(id16(0xC4, 0, 1));
    let prov_healthy_id = FanoutProviderId::from_bytes(id16(0xC4, 0, 2));
    let tcb_rg = ResourceGroupId::from_bytes(id16(0xC5, 0, 0));

    let mut bulkhead = BulkheadController::new(
        32,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 4,
        },
    )
    .expect("create bulkhead controller");
    let budget = FailureBudget {
        max_consecutive_failures: 8,
        max_total_failures: 16,
    };
    for (scope_id, cap, queue_cap, tag) in [
        (BulkheadScopeId::Tenant(tenant_id), 24_u32, 16_u32, 0xC6_u8),
        (BulkheadScopeId::Workspace(workspace_id), 24, 16, 0xC7),
        (BulkheadScopeId::TaskGroup(group_id), 24, 16, 0xC8),
        (BulkheadScopeId::Provider(prov_stalled_id), 4, 4, 0xC9),
        (BulkheadScopeId::Provider(prov_healthy_id), 16, 16, 0xCA),
    ] {
        bulkhead
            .register_bulkhead(BulkheadSpec {
                scope_id,
                resource_group_id: ResourceGroupId::from_bytes(id16(tag, 0, 1)),
                max_concurrent: cap,
                max_queue_depth: queue_cap,
                failure_budget: budget,
            })
            .expect("register provider stall bulkhead");
    }
    let bulkhead = Arc::new(Mutex::new(bulkhead));

    let stalled_route = WorkloadBulkheadRoute {
        tenant_id,
        workspace_id,
        task_group_id: group_id,
        provider_id: Some(prov_stalled_id),
    };
    let healthy_route = WorkloadBulkheadRoute {
        tenant_id,
        workspace_id,
        task_group_id: group_id,
        provider_id: Some(prov_healthy_id),
    };

    // 1) Launch 16 requests to `provider_stalled`:
    //    - 4 acquire `Provider(prov_stalled_id)` slots, dispatch durably, and stall in `pending()`,
    //    - 4 queue specifically on `Provider(prov_stalled_id)`,
    //    - 8 shed immediately (`RejectedQueueFull`) without pinning Tenant/Workspace/TaskGroup!
    let mut stalled_in_flight = Vec::new();
    let mut queued_on_stalled = 0_u64;
    let mut shed_on_stalled = 0_u64;

    for seq in 0..16_u64 {
        let adm = bulkhead
            .lock()
            .expect("lock bulkhead")
            .acquire_workload(stalled_route)
            .expect("evaluate stalled provider route");
        match adm {
            BulkheadAdmissionDecision::Acquired { lease_id, .. } => {
                let spec = make_fiber_spec(0xCB, 0, seq, Generation::INITIAL);
                let cancel_scope = spec.cancellation_scope_id;
                let fiber_handle = adapter
                    .spawn_fiber(spec, Box::pin(pending()))
                    .expect("spawn stalled slow fiber");
                let reg = stalled_provider
                    .register(OperationSpec {
                        operation_id: OperationId::from_bytes(id16(0xCC, 0, seq)),
                        generation: Generation::INITIAL,
                        owner_fiber: fiber_handle,
                        cancellation_scope_id: cancel_scope,
                        cancellation_generation: Generation::INITIAL,
                    })
                    .expect("register stalled op");
                let cb_id = CallbackId::from_bytes(id16(0xCD, 0, seq));
                let disp = stalled_provider
                    .dispatch(DispatchProviderOperation {
                        handle: reg.handle,
                        callback_id: cb_id,
                    })
                    .expect("durable dispatch before stall");
                assert!(!disp.replayed);
                stalled_in_flight.push((lease_id, fiber_handle, cancel_scope, reg.handle, cb_id));
            }
            BulkheadAdmissionDecision::Queued {
                bottleneck_scope, ..
            } => {
                assert_eq!(bottleneck_scope, BulkheadScopeId::Provider(prov_stalled_id));
                queued_on_stalled += 1;
            }
            BulkheadAdmissionDecision::RejectedQueueFull { saturated_scope } => {
                assert_eq!(saturated_scope, BulkheadScopeId::Provider(prov_stalled_id));
                shed_on_stalled += 1;
            }
            other => panic!("unexpected stalled bulkhead decision: {other:?}"),
        }
    }
    assert_eq!(stalled_in_flight.len(), 4);
    assert_eq!(queued_on_stalled, 4);
    assert_eq!(shed_on_stalled, 8);

    // Arm RPC fault on `stalled_provider` to verify unreachable RPCs fail closed without corrupting DB.
    stalled_provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    assert!(matches!(
        stalled_provider.complete(CompleteProviderOperation {
            handle: stalled_in_flight[0].3,
            callback_id: stalled_in_flight[0].4,
            seed: [0x11; 32],
        }),
        Err(ProviderError::Unreachable)
    ));

    // 2) While `provider_stalled` is hung, run 128 `Fast` fibers AND 24 durable `Slow` operations
    //    on `provider_healthy` concurrently!
    let fast_task = {
        let rt = adapter.clone();
        tokio::spawn(async move {
            let mut lats = Vec::with_capacity(128);
            for seq in 0..128_u64 {
                let t0 = Instant::now();
                let (tx, rx) = tokio::sync::oneshot::channel::<u64>();
                let h = rt
                    .spawn_fiber(
                        make_fiber_spec(0xCE, 1, seq, Generation::INITIAL),
                        Box::pin(async move {
                            let _ = tx.send(seq + 1);
                            FiberExit::Completed
                        }),
                    )
                    .expect("spawn concurrent fast fiber");
                assert_eq!(rx.await.expect("fast rx"), seq + 1);
                let rt_join = rt.clone();
                let exit = tokio::task::spawn_blocking(move || rt_join.join_fiber(h))
                    .await
                    .expect("blocking join")
                    .expect("join fast fiber");
                assert_eq!(exit, FiberExit::Completed);
                lats.push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
            lats
        })
    };

    let healthy_slow_task = {
        let bh = Arc::clone(&bulkhead);
        let prov = Arc::clone(&healthy_provider);
        let store = Arc::clone(&healthy_store);
        tokio::task::spawn_blocking(move || {
            let mut lats = Vec::with_capacity(24);
            for seq in 0..24_u64 {
                let t0 = Instant::now();
                let BulkheadAdmissionDecision::Acquired { lease_id, .. } = bh
                    .lock()
                    .expect("lock bh")
                    .acquire_workload(healthy_route)
                    .expect("acquire healthy provider slot")
                else {
                    panic!("healthy provider pool must remain unblocked during sibling stall");
                };
                let owner = FiberHandle {
                    fiber_id: ExecutionFiberId::from_bytes(id16(0xCF, 2, seq)),
                    generation: Generation::INITIAL,
                };
                let reg = prov
                    .register(OperationSpec {
                        operation_id: OperationId::from_bytes(id16(0xD0, 2, seq)),
                        generation: Generation::INITIAL,
                        owner_fiber: owner,
                        cancellation_scope_id: CancellationScopeId::from_bytes(id16(0xD1, 2, seq)),
                        cancellation_generation: Generation::INITIAL,
                    })
                    .expect("healthy register");
                let cb = CallbackId::from_bytes(id16(0xD2, 2, seq));
                prov.dispatch(DispatchProviderOperation {
                    handle: reg.handle,
                    callback_id: cb,
                })
                .expect("healthy dispatch");
                prov.complete(CompleteProviderOperation {
                    handle: reg.handle,
                    callback_id: cb,
                    seed: digest32(0xD3, 2, seq),
                })
                .expect("healthy complete");
                let pending_rows = store.pending_outbox(4).expect("healthy outbox");
                for row in pending_rows {
                    store.acknowledge_outbox(row.sequence).expect("ack healthy");
                }
                bh.lock()
                    .expect("lock bh")
                    .complete_workload_lease(lease_id, &BulkheadExecutionOutcome::Success, 5_000)
                    .expect("release healthy lease");
                lats.push(u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX));
            }
            lats
        })
    };

    let (fast_lats, healthy_slow_lats) = tokio::join!(fast_task, healthy_slow_task);
    let fast_stats = summarize_ns_as_us(&fast_lats.expect("fast task"));
    let healthy_slow_stats = summarize_ns_as_us(&healthy_slow_lats.expect("healthy slow task"));

    // 3) Deadline / Cancel fence on the 4 stalled operations:
    //    Cancel their scopes, fence the durable operations (`CancelRequested`), reap all fibers,
    //    then disarm `stalled_provider` and verify late completion routes to `ReconcileEffect`
    //    (NEVER `WakeFiber`)!
    stalled_provider.disarm_fault();
    let mut reconcile_fenced_count = 0_u64;
    for (idx, (lease_id, fiber_handle, cancel_scope, op_handle, cb_id)) in
        stalled_in_flight.into_iter().enumerate()
    {
        let seq = u64::try_from(idx).unwrap_or(0);
        let cancel_dec = stalled_store
            .request_cancel_idempotent(
                op_handle,
                CancelEpoch::INITIAL,
                ReceiptId::from_bytes(id16(0xD4, 0, seq)),
            )
            .expect("durable cancel fence on stalled op");
        assert!(matches!(
            cancel_dec,
            CancelRequestDecision::Applied(snap) if snap.state == OperationState::CancelRequested
        ));

        adapter
            .cancel_scope(cancel_scope, Generation::INITIAL)
            .expect("cancel stalled fiber scope");
        let rt_join = adapter.clone();
        let exit = tokio::task::spawn_blocking(move || rt_join.join_fiber(fiber_handle))
            .await
            .expect("join blocking")
            .expect("reap stalled fiber");
        assert_eq!(exit, FiberExit::Cancelled);

        // Late callback arrives after cancel -> fenced into `OutboxKind::ReconcileEffect`!
        let late = stalled_provider
            .complete(CompleteProviderOperation {
                handle: op_handle,
                callback_id: cb_id,
                seed: digest32(0xD5, 0, seq),
            })
            .expect("late completion canonicalized for reconciliation");
        assert!(!late.replayed);
        let _ = bulkhead.lock().expect("lock bh").complete_workload_lease(
            lease_id,
            &BulkheadExecutionOutcome::Failure {
                attributed_scopes: vec![BulkheadScopeId::Provider(prov_stalled_id)],
            },
            6_000,
        );
    }

    let outbox_entries = stalled_store
        .pending_outbox(16)
        .expect("inspect stalled outbox");
    assert_eq!(outbox_entries.len(), 4);
    for entry in &outbox_entries {
        assert_eq!(
            entry.kind,
            OutboxKind::ReconcileEffect,
            "late completion after timeout/cancel must fence wake and route to ReconcileEffect"
        );
        reconcile_fenced_count += 1;
    }

    assert_eq!(
        adapter.registered_fibers(),
        0,
        "zero leaked fibers/threads after provider stall timeout & cancel"
    );
    assert_eq!(bulkhead.lock().expect("lock bh").available_tcb_slots(), 4);

    println!(
        "[c-bench] fault=provider_stall stalled_active=4 stalled_queued={} stalled_shed={} late_callbacks_fenced_to_reconcile={}/4 leaked_fibers={} fast_lat_us(p50={},p95={},p99={}) healthy_provider_slow_us(p50={},p95={},p99={})",
        queued_on_stalled,
        shed_on_stalled,
        reconcile_fenced_count,
        adapter.registered_fibers(),
        fast_stats.p50_us,
        fast_stats.p95_us,
        fast_stats.p99_us,
        healthy_slow_stats.p50_us,
        healthy_slow_stats.p95_us,
        healthy_slow_stats.p99_us,
    );
    assert!(
        fast_stats.p95_us < healthy_slow_stats.p95_us && fast_stats.p95_us < 25_000,
        "Fast path p95 ({} us) must remain unaffected and beat durable Slow p95 ({} us) during provider stall",
        fast_stats.p95_us,
        healthy_slow_stats.p95_us,
    );
}

// ============================================================================
// 5. [ROAD-C-002] Fault Scenario 5: Cell Bulkhead Flood, DRR Fairness & Offload
// ============================================================================

#[test]
fn road_c002_fault_cell_bulkhead_preserves_safety_tcb_drr_fairness_and_offloads_healthy_cells() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_DOMAIN_BYTES, 128);
    let mut cells = Vec::with_capacity(CLUSTER_CELL_SHARDS);
    let mut workers = Vec::with_capacity(CLUSTER_CELL_SHARDS);

    for s_idx in 0..CLUSTER_CELL_SHARDS {
        let cell_dom = scheduler.add_cell(cell_nominal_id(s_idx), 16);
        // RG_PRIMARY = flooded noisy group (weight = 1); RG_SECONDARY = fair sibling group (weight = 3).
        cell_dom.register_resource_group(ResourceGroupQueuePolicy {
            resource_group_id: RG_PRIMARY,
            max_queue_depth: 32,
            parent_hard_quota_in_flight: 32,
            weight: 1,
            max_priority_cap: 150,
            starvation_aging_step_ms: 50,
        });
        cell_dom.register_resource_group(ResourceGroupQueuePolicy {
            resource_group_id: RG_SECONDARY,
            max_queue_depth: 32,
            parent_hard_quota_in_flight: 32,
            weight: 3,
            max_priority_cap: 150,
            starvation_aging_step_ms: 50,
        });
        let worker = cell_dom.add_worker(
            worker_nominal_id(s_idx, 0),
            WorkerDomainConfig {
                max_concurrency: 16,
                base_materialization_window: 16,
            },
            default_worker_capabilities(),
        );
        cells.push(cell_dom);
        workers.push(worker);
    }

    // Cell 0 BulkheadController: 6 untouchable SafetyTcbSystemReserve slots,
    // `group_flood` capped at 4 concurrent + 4 queued, `group_fair` capped at 8 concurrent + 8 queued.
    let tcb_rg = ResourceGroupId::from_bytes(id16(0xE0, 0, 0));
    let mut cell0_bulkhead = BulkheadController::new(
        24,
        SafetyTcbSystemReserve {
            resource_group_id: tcb_rg,
            reserved_tcb_slots: 6,
        },
    )
    .expect("create cell 0 bulkhead");
    let tenant_id = FanoutTenantId::from_bytes(id16(0xE1, 0, 1));
    let workspace_id = WorkspaceId::from_bytes(id16(0xE2, 0, 1));
    let group_flood = TaskGroupId::from_bytes(id16(0xE3, 0, 1));
    let group_fair = TaskGroupId::from_bytes(id16(0xE3, 0, 2));
    let budget = FailureBudget {
        max_consecutive_failures: 5,
        max_total_failures: 10,
    };
    for (scope_id, cap, queue_cap, tag) in [
        (BulkheadScopeId::Tenant(tenant_id), 20_u32, 16_u32, 0xE4_u8),
        (BulkheadScopeId::Workspace(workspace_id), 20, 16, 0xE5),
        (BulkheadScopeId::TaskGroup(group_flood), 4, 4, 0xE6),
        (BulkheadScopeId::TaskGroup(group_fair), 8, 8, 0xE7),
    ] {
        cell0_bulkhead
            .register_bulkhead(BulkheadSpec {
                scope_id,
                resource_group_id: ResourceGroupId::from_bytes(id16(tag, 0, 1)),
                max_concurrent: cap,
                max_queue_depth: queue_cap,
                failure_budget: budget,
            })
            .expect("register cell 0 bulkhead scope");
    }

    // 1) Flood `group_flood` on Cell 0 with 200 bursts!
    let flood_route = WorkloadBulkheadRoute {
        tenant_id,
        workspace_id,
        task_group_id: group_flood,
        provider_id: None,
    };
    let mut flood_acquired = 0_u64;
    let mut flood_queued = 0_u64;
    let mut flood_shed = 0_u64;
    for _ in 0..200_u32 {
        match cell0_bulkhead
            .acquire_workload(flood_route)
            .expect("evaluate flood")
        {
            BulkheadAdmissionDecision::Acquired { .. } => flood_acquired += 1,
            BulkheadAdmissionDecision::Queued { .. } => flood_queued += 1,
            BulkheadAdmissionDecision::RejectedQueueFull { saturated_scope } => {
                assert_eq!(saturated_scope, BulkheadScopeId::TaskGroup(group_flood));
                flood_shed += 1;
            }
            other => panic!("unexpected flood decision: {other:?}"),
        }
    }
    assert_eq!(flood_acquired, 4);
    assert_eq!(flood_queued, 4);
    assert_eq!(flood_shed, 192);

    // 2) Verify `SafetyTcbSystemReserve` is 100% untouched (all 6/6 slots immediately acquirable)!
    assert_eq!(cell0_bulkhead.available_tcb_slots(), 6);
    let mut tcb_leases = Vec::with_capacity(6);
    for _ in 0..6 {
        let (tcb_lease, rg) = cell0_bulkhead
            .acquire_safety_tcb_slot()
            .expect("SafetyTcbSystemReserve must never be touched by Cell bulkhead flood");
        assert_eq!(rg, tcb_rg);
        tcb_leases.push(tcb_lease);
    }
    assert_eq!(cell0_bulkhead.available_tcb_slots(), 0);
    for tcb_lease in tcb_leases {
        cell0_bulkhead
            .release_safety_tcb_slot(tcb_lease)
            .expect("release TCB slot");
    }
    assert_eq!(cell0_bulkhead.available_tcb_slots(), 6);

    // 3) Verify sibling `group_fair` on Cell 0 acquires all 8 slots unimpeded AND DRR weight (3:1)
    //    prevents starvation when both `RG_PRIMARY` (flood, w=1) and `RG_SECONDARY` (fair, w=3)
    //    contend at equal priority on Cell 0's worker!
    let fair_route = WorkloadBulkheadRoute {
        tenant_id,
        workspace_id,
        task_group_id: group_fair,
        provider_id: None,
    };
    for _ in 0..8 {
        assert!(matches!(
            cell0_bulkhead
                .acquire_workload(fair_route)
                .expect("fair sibling acquire"),
            BulkheadAdmissionDecision::Acquired { .. }
        ));
    }

    for seq in 0..12_u64 {
        assert!(
            workers[0]
                .enqueue(
                    make_work_item(0xE8, 0, seq, RG_PRIMARY, 100, DeadlineClass::Interactive, 0),
                    0,
                )
                .is_admitted()
        );
        assert!(
            workers[0]
                .enqueue(
                    make_work_item(
                        0xE9,
                        0,
                        seq,
                        RG_SECONDARY,
                        100,
                        DeadlineClass::Interactive,
                        0,
                    ),
                    0,
                )
                .is_admitted()
        );
    }

    // Dequeue first 12 items from Cell 0's worker -> `RG_SECONDARY` (weight=3) vs `RG_PRIMARY` (weight=1)
    // yields exact 9:3 DRR fairness!
    let mut first_12_flood = 0_u64;
    let mut first_12_fair = 0_u64;
    for _ in 0..12 {
        let d = workers[0].dequeue_next(10).expect("dequeue DRR sample");
        workers[0].complete_in_flight(d.item.resource_group_id);
        if d.item.resource_group_id == RG_PRIMARY {
            first_12_flood += 1;
        } else {
            first_12_fair += 1;
        }
    }
    assert!(
        first_12_fair >= first_12_flood * 2 && first_12_flood >= 1,
        "DRR must honor 3:1 weight without starving either group (got fair={first_12_fair}, flood={first_12_flood})"
    );
    while let Some(d) = workers[0].dequeue_next(10) {
        workers[0].complete_in_flight(d.item.resource_group_id);
    }

    // 4) Saturate Cell 0 (`ModelSlot`) -> verify `Cell 0 -> Global` reverse backpressure
    //    contracts Global's materialization window AND offloads 64/64 new placements targeting
    //    `Some(cell_0)` onto healthy Cells `1..7`!
    let initial_global_window = scheduler.global().effective_materialization_window();
    workers[0].set_saturation_source(SaturationSource::ModelSlot, true);

    let cell0_pressure = scheduler
        .global()
        .inspect_cell_pressure(cell_nominal_id(0))
        .expect("inspect cell 0 pressure");
    assert_eq!(cell0_pressure.level, DomainPressureLevel::Saturated);
    assert_eq!(
        cell0_pressure.primary_source,
        Some(SaturationSource::ModelSlot)
    );
    let contracted_global_window = scheduler.global().effective_materialization_window();
    assert!(
        contracted_global_window < initial_global_window,
        "Cell 0 saturation must contract Global materialization window ({contracted_global_window} < {initial_global_window})"
    );

    let cell0_worker_domain = workers[0].domain_id();
    let mut offloaded_to_healthy_cells = 0_u64;
    for seq in 0..64_u64 {
        let item = make_work_item(
            0xEA,
            0,
            seq,
            RG_SECONDARY,
            110,
            DeadlineClass::Interactive,
            100 + seq,
        );
        let adm = scheduler.place_across_cells(item, Some(cell_nominal_id(0)), 100 + seq);
        let SchedulerAdmission::Admitted {
            assigned_domain, ..
        } = adm
        else {
            panic!("expected offload admission onto healthy Cells 1..7, got {adm:?}");
        };
        assert_ne!(
            assigned_domain, cell0_worker_domain,
            "saturated Cell 0 must never receive shed placements while healthy Cells 1..7 have headroom"
        );
        offloaded_to_healthy_cells += 1;
    }
    assert_eq!(offloaded_to_healthy_cells, 64);
    assert_eq!(cells[0].domain_id().cell_id(), Some(cell_nominal_id(0)));

    println!(
        "[c-bench] fault=cell_bulkhead flood_acquired={flood_acquired} flood_queued={flood_queued} flood_shed={flood_shed} tcb_reserved_untouched=6/6 drr_share_fair_vs_flood={first_12_fair}:{first_12_flood} global_window_contracted={initial_global_window}->{contracted_global_window} cross_cell_offloaded={offloaded_to_healthy_cells}/64"
    );
}
