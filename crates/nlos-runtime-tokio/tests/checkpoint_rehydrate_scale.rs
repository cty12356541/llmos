//! Explicit W31-C checkpoint/rehydrate benchmark over the ADR-0012 decision-2
//! B path: N fiber nodes each checkpoint their handler-entry input through
//! [`TokioRuntimeAdapter::snapshot_handler_entry`], the whole runtime is
//! torn down (the evict: every in-memory fiber disappears while the durable
//! `fiber_entry_snapshots` survive), and a fresh runtime rehydrates every
//! node on demand through [`TokioRuntimeAdapter::resume_from_snapshot`],
//! whose handler re-executes from the entry input and re-registers its
//! durable Channel wait through the normal runtime entry
//! (`tests/fiber_replay_registration.rs` semantics at scale).
//!
//! Measured and recorded (single platform, verbatim in
//! `docs/evidence/stage-b/b-task-scale-001.md` §W31-C):
//!
//! 1. checkpoint: per-node and total wall time of the durable entry
//!    snapshot writes (process binding + incarnation + snapshot per node),
//! 2. rehydrate: per-node and total wall time of the on-demand restore
//!    (next incarnation + resume + the handler's durable wait
//!    re-registration), plus derived throughput,
//! 3. durable bytes of the authority databases after each phase, and RSS
//!    where a portable read exists.
//!
//! Hard-bound assertions only: every node checkpoints, every node
//! rehydrates with `restored: Some`, every rehydrated fiber parks at
//! `WaitingIo` with a `PENDING` durable wait row, and the phase totals stay
//! under generous ceilings. Percentiles are recorded, never asserted.
//!
//! Tiers: the default-suite tier keeps a modest node count so the regular
//! suite stays fast; the quick and full tiers are `#[ignore]`-gated like
//! the other scale probes and run via `--ignored`.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_process::{
    CreateIsolationDomainRequest, FiberIncarnationDecision, IsolationDomainDecision,
    ProcessAuthority, ProcessBindingDecision, RegisterDelegatedProcessRequest,
    RegisterFiberIncarnationRequest,
};
use nlos_runtime::{FiberHandle, FiberSpec, FiberState, RuntimeAdapter};
use nlos_runtime_tokio::{
    ChannelSequenceWait, SnapshotResumable, TokioRuntimeAdapter, TokioRuntimeConfig,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    IsolationDomainId, ProcessId, ResourceGroupId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use nlos_wait::{BindingId, RegisterWaitRequest, WaitAuthority, WaitState};
use tokio::runtime::Handle;

/// Default-suite tier: proves the checkpoint/evict/rehydrate loop on every
/// regular run without slowing the suite.
const SMALL_NODES: usize = 24;
/// Quick ignored tier for fast local re-measurement.
const QUICK_NODES: usize = 500;
/// Full ignored tier.
const FULL_NODES: usize = 5_000;
/// Handler-entry input size per node (bytes).
const HANDLER_INPUT_BYTES: usize = 64;
/// Durable wait target sequence every rehydrated handler re-registers.
const WAIT_TARGET_SEQUENCE: u64 = 5;

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0x37_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn key(domain: u8, index: usize) -> IdempotencyKey {
    let mut bytes = [domain; 16];
    bytes[8..].copy_from_slice(&(index as u64).to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

fn node_binding(index: usize) -> BindingId {
    BindingId::from_bytes(id_bytes(index))
}

fn node_fiber(index: usize) -> ExecutionFiberId {
    ExecutionFiberId::from_bytes(id_bytes(index))
}

fn node_handler_input(index: usize) -> Vec<u8> {
    let mut input = vec![0x5a_u8; HANDLER_INPUT_BYTES];
    input[8..16].copy_from_slice(&(index as u64).to_be_bytes());
    input
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-runtime-tokio-checkpoint-rehydrate-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Authorities {
    channel: Arc<ChannelAuthority>,
    wait: Arc<WaitAuthority>,
    process: Arc<ProcessAuthority>,
}

fn open_authorities(root: &Root) -> Authorities {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority"),
    );
    let process = Arc::new(ProcessAuthority::open(root.path()).expect("open process authority"));
    Authorities {
        channel,
        wait,
        process,
    }
}

/// Durable footprint of the authority root: every database and WAL file,
/// excluding the transient `-shm` shared-memory sidecars.
fn durable_bytes(root: &Root) -> u64 {
    fs_metadata_dir(root.path())
        .expect("list authority root")
        .into_iter()
        .filter(|(name, _)| !name.ends_with("-shm"))
        .map(|(_, len)| len)
        .sum()
}

fn fs_metadata_dir(path: &Path) -> Result<Vec<(String, u64)>, ()> {
    let mut entries = Vec::new();
    let read = std::fs::read_dir(path).map_err(|_| ())?;
    for entry in read.flatten() {
        if let Ok(metadata) = entry.metadata()
            && metadata.is_file()
        {
            entries.push((
                entry.file_name().to_string_lossy().into_owned(),
                metadata.len(),
            ));
        }
    }
    Ok(entries)
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

fn fiber_spec(index: usize, generation: Generation) -> FiberSpec {
    FiberSpec {
        fiber_id: node_fiber(index),
        fiber_generation: generation,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id_bytes(index)),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(index)),
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

fn runtime(max_live_fibers: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

fn register_process(
    process: &ProcessAuthority,
    index: usize,
) -> (ProcessId, Generation, Generation) {
    let domain = match process.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [0x11; 32],
        idempotency_key: key(0x10, index),
        created_at_ms: 900,
    }) {
        Ok(IsolationDomainDecision::Created(record)) => record,
        other => panic!("expected Created domain, got {other:?}"),
    };
    let binding = match process.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes(id_bytes(index)),
        task_attempt_id: TaskAttemptId::from_bytes(id_bytes(index)),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: IsolationDomainId::from_bytes(domain.isolation_domain_id.into_bytes()),
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: key(0x20, index),
        created_at_ms: 950,
    }) {
        Ok(ProcessBindingDecision::Registered(record)) => record,
        other => panic!("expected Registered binding, got {other:?}"),
    };
    let incarnation = match process.register_fiber_incarnation(RegisterFiberIncarnationRequest {
        process_id: binding.process_id,
        expected_process_generation: binding.process_generation,
        expected_process_fencing_token: binding.process_fencing_token,
        binding: node_fiber(index),
        idempotency_key: key(0x30, index),
        registered_at_ms: 990,
    }) {
        Ok(FiberIncarnationDecision::Registered(record)) => record,
        other => panic!("expected Registered incarnation, got {other:?}"),
    };
    (
        binding.process_id,
        binding.process_generation,
        incarnation.incarnation_generation,
    )
}

fn next_incarnation(
    process: &ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    index: usize,
) -> Generation {
    let head = process
        .inspect_active_process_binding(process_id)
        .expect("process head");
    assert_eq!(head.process_generation, process_generation);
    match process.register_fiber_incarnation(RegisterFiberIncarnationRequest {
        process_id,
        expected_process_generation: head.process_generation,
        expected_process_fencing_token: head.process_fencing_token,
        binding: node_fiber(index),
        idempotency_key: key(0x40, index),
        registered_at_ms: 1_900,
    }) {
        Ok(FiberIncarnationDecision::Registered(record)) => record.incarnation_generation,
        other => panic!("expected Registered next incarnation, got {other:?}"),
    }
}

/// The B-path handler every node re-executes on rehydrate: asserts the
/// restored entry input, then drives itself back to its wait point by
/// re-registering its durable Channel wait through the normal runtime
/// entry.
struct RehydratingHandler {
    index: usize,
    process: ProcessId,
    incarnation: Generation,
    handle: FiberHandle,
    adapter: TokioRuntimeAdapter,
    waits: Arc<WaitAuthority>,
    channel_id: nlos_types::ChannelId,
    armed: std::sync::Mutex<Vec<ChannelSequenceWait>>,
}

impl SnapshotResumable for RehydratingHandler {
    fn binding(&self) -> BindingId {
        node_binding(self.index)
    }

    fn process_id(&self) -> ProcessId {
        self.process
    }

    fn expected_incarnation(&self) -> Generation {
        self.incarnation
    }

    fn handler_input(&self) -> Vec<u8> {
        node_handler_input(self.index)
    }

    fn resume_from_entry(&self, input: &[u8]) -> Result<(), nlos_runtime_tokio::ResumeRejection> {
        assert_eq!(input, node_handler_input(self.index));
        let wait = self
            .adapter
            .wait_for_channel(
                self.handle,
                &self.waits,
                RegisterWaitRequest {
                    binding: node_binding(self.index),
                    channel_id: self.channel_id,
                    target_sequence: WAIT_TARGET_SEQUENCE,
                    idempotency_key: key(0x50, self.index),
                    registered_at_ms: 2_500,
                },
            )
            .map_err(|error| nlos_runtime_tokio::ResumeRejection {
                reason: format!("re-registration failed: {error}"),
            })?;
        self.armed.lock().expect("armed cell").push(wait);
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct PhaseStats {
    p50: Duration,
    p95: Duration,
    max: Duration,
}

fn phase_stats(mut latencies: Vec<Duration>) -> PhaseStats {
    latencies.sort_unstable();
    let len = latencies.len();
    PhaseStats {
        p50: latencies[len / 2],
        p95: latencies[(len * 95) / 100],
        max: latencies[len - 1],
    }
}

/// Given/When/Then: given N fiber nodes; when each checkpoints its
/// handler-entry input through the public B-path write, the whole runtime
/// is dropped (the evict), and a fresh runtime rehydrates every node on
/// demand through the public B-path restore; then every restore returns
/// its snapshot, every rehydrated fiber parks at `WaitingIo` with a
/// `PENDING` durable wait row, and both phase totals stay under the
/// generous ceilings — with checkpoint/rehydrate latency percentiles,
/// throughput, durable bytes, and RSS recorded for the evidence file.
#[allow(clippy::too_many_lines)]
#[allow(clippy::cast_precision_loss)] // Throughput display only: node count into f64 seconds.
fn run_tier(label: &str, nodes: usize, ceiling: Duration) {
    let root = Root::new(label);
    let authorities = open_authorities(&root);
    let channel = match authorities.channel.create_channel(CreateChannelRequest {
        capacity_bytes: 4096,
        policy_digest: [0x44; 32],
        idempotency_key: key(0x01, 0),
        created_at_ms: 900,
    }) {
        Ok(ChannelDecision::Created(record) | ChannelDecision::Replayed(record)) => record,
        Err(error) => panic!("create channel: {error}"),
    };

    // -- Checkpoint phase: per node, the durable process/incarnation
    //    registrations plus the public entry-snapshot write.
    let rss_before = sample_rss_bytes();
    let adapter = runtime(nodes + 8);
    let mut checkpoint_latencies = Vec::with_capacity(nodes);
    let mut nodes_meta = Vec::with_capacity(nodes);
    let checkpoint_started = Instant::now();
    for index in 0..nodes {
        let node_started = Instant::now();
        let (process_id, process_generation, incarnation) =
            register_process(&authorities.process, index);
        let (handle, _scope) = {
            let scope = CancellationScopeId::from_bytes(id_bytes(index));
            let handle = adapter
                .spawn_fiber(fiber_spec(index, Generation::INITIAL), Box::pin(pending()))
                .expect("spawn checkpoint fiber");
            (handle, scope)
        };
        let handler = RehydratingHandler {
            index,
            process: process_id,
            incarnation,
            handle,
            adapter: adapter.clone(),
            waits: Arc::clone(&authorities.wait),
            channel_id: channel.channel_id,
            armed: std::sync::Mutex::new(Vec::new()),
        };
        let record = adapter
            .snapshot_handler_entry(handle, &authorities.process, &handler)
            .expect("checkpoint write")
            .expect("live fiber records its entry");
        assert_eq!(record.handler_input, node_handler_input(index));
        checkpoint_latencies.push(node_started.elapsed());
        nodes_meta.push((process_id, process_generation));
    }
    let checkpoint_total = checkpoint_started.elapsed();
    let checkpoint_stats = phase_stats(checkpoint_latencies);
    let bytes_after_checkpoint = durable_bytes(&root);

    // -- Evict phase: the whole runtime goes away; the durable snapshots
    //    and process facts survive in the authority databases alone.
    drop(adapter);
    let rss_after_evict = sample_rss_bytes();

    // -- Rehydrate phase: fresh runtime, one next incarnation per node, and
    //    the public B-path restore driving each handler back to its wait
    //    point on demand.
    let rehydrate_adapter = runtime(nodes + 8);
    let armed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut rehydrate_latencies = Vec::with_capacity(nodes);
    let rehydrate_started = Instant::now();
    for (index, &(process_id, process_generation)) in nodes_meta.iter().enumerate() {
        let node_started = Instant::now();
        let incarnation =
            next_incarnation(&authorities.process, process_id, process_generation, index);
        let handle = rehydrate_adapter
            .spawn_fiber(
                fiber_spec(index, Generation::INITIAL.checked_next().expect("next")),
                Box::pin(pending()),
            )
            .expect("spawn rehydrated fiber");
        let handler = RehydratingHandler {
            index,
            process: process_id,
            incarnation,
            handle,
            adapter: rehydrate_adapter.clone(),
            waits: Arc::clone(&authorities.wait),
            channel_id: channel.channel_id,
            armed: std::sync::Mutex::new(Vec::new()),
        };
        let report = rehydrate_adapter
            .resume_from_snapshot(handle, &authorities.process, &handler)
            .expect("rehydrate node")
            .restored
            .expect("every evicted node restores its snapshot");
        assert_eq!(report.handler_input, node_handler_input(index));
        assert_eq!(
            rehydrate_adapter.inspect(handle).expect("inspect"),
            FiberState::WaitingIo,
            "rehydrated node {index} must park at its wait point"
        );
        let handler_armed: Vec<_> = handler
            .armed
            .lock()
            .expect("armed cell")
            .drain(..)
            .collect();
        assert_eq!(handler_armed.len(), 1);
        armed.lock().expect("armed cell").extend(handler_armed);
        rehydrate_latencies.push(node_started.elapsed());
    }
    let rehydrate_total = rehydrate_started.elapsed();
    let rehydrate_stats = phase_stats(rehydrate_latencies);
    let bytes_after_rehydrate = durable_bytes(&root);
    let rss_after = sample_rss_bytes();

    // Hard-bound verification: every durable wait row the rehydrated
    // handlers registered is `PENDING` with the node's binding.
    let armed_count = armed.lock().expect("armed cell").len();
    assert_eq!(armed_count, nodes);
    for index in 0..nodes {
        let rows = authorities
            .wait
            .list_waits_for_binding(node_binding(index))
            .expect("list node waits");
        assert_eq!(rows.len(), 1, "node {index}");
        assert_eq!(rows[0].state, WaitState::Pending);
        assert_eq!(rows[0].target_sequence, WAIT_TARGET_SEQUENCE);
    }

    assert!(
        checkpoint_total < ceiling,
        "{label} checkpoint: {checkpoint_total:?} exceeded ceiling {ceiling:?}"
    );
    assert!(
        rehydrate_total < ceiling,
        "{label} rehydrate: {rehydrate_total:?} exceeded ceiling {ceiling:?}"
    );

    let rehydrates_per_second = nodes as f64 / rehydrate_total.as_secs_f64();
    eprintln!(
        "checkpoint/rehydrate benchmark ({label}, single platform): nodes={nodes} \
         input_bytes={HANDLER_INPUT_BYTES} \
         checkpoint_total={checkpoint_total:?} checkpoint_p50={:?} checkpoint_p95={:?} \
         checkpoint_max={:?} \
         rehydrate_total={rehydrate_total:?} rehydrate_p50={:?} rehydrate_p95={:?} \
         rehydrate_max={:?} rehydrates_per_second={rehydrates_per_second:.1} \
         durable_bytes_after_checkpoint={bytes_after_checkpoint} \
         durable_bytes_after_rehydrate={bytes_after_rehydrate} \
         rss_before={rss_before:?} rss_after_evict={rss_after_evict:?} rss_after={rss_after:?}",
        checkpoint_stats.p50,
        checkpoint_stats.p95,
        checkpoint_stats.max,
        rehydrate_stats.p50,
        rehydrate_stats.p95,
        rehydrate_stats.max,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn small_tier_checkpoint_evict_rehydrate_round_trip() {
    run_tier("small", SMALL_NODES, Duration::from_mins(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit W31-C checkpoint/rehydrate benchmark (quick tier)"]
async fn quick_tier_checkpoint_evict_rehydrate_round_trip() {
    run_tier("quick", QUICK_NODES, Duration::from_mins(10));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit W31-C checkpoint/rehydrate benchmark (full tier)"]
async fn full_tier_checkpoint_evict_rehydrate_round_trip() {
    run_tier("full", FULL_NODES, Duration::from_mins(20));
}
