//! W25 FIBER-REAP lifecycle reaping tests for [`TokioRuntimeAdapter`].
//!
//! Spec: `docs/superpowers/specs/2026-09-12-fiber-lifecycle-reaping-design.md`
//! (normative statements FIBER-REAP-001..005 and §2.4 zero-capacity
//! semantics). Coverage map:
//!
//! - FIBER-REAP-001 / FIBER-REAP-002: a successful join consumes the fiber
//!   record inside the terminal critical section (registry count drops) and
//!   a re-join of the reaped `(fiber_id, generation)` reports
//!   [`RuntimeError::FiberReaped`];
//! - FIBER-REAP-003: detach on an already-terminal record reaps immediately;
//!   detach on a live record reaps at the terminal transition;
//! - FIBER-REAP-004: a re-spawn of a reaped identity stays `DuplicateFiber`
//!   while its tombstone is inside the FIFO ring, and becomes allowed once
//!   evicted (`tombstone_capacity = 1`);
//! - §2.4: `tombstone_capacity = 0` is a zero-capacity ring — pure
//!   consumption with no window protection.
//!
//! The scope group below covers SCOPE-IDX-001..004: the by-id index, the
//! entry reference count reaped with the last referencing fiber record, and
//! the bounded scope tombstone ring. The orphan buffer bound (ORPHAN-*) is a
//! separate W25 task and is not covered here.
//!
//! R1 pins (W25-001R): `FiberReaped` reaches every handle-addressed entry
//! through the shared record resolution, not just `join_fiber` — the
//! Operation-wait registration, the inherent lifecycle inspection helper,
//! and the durable snapshot-family GC are pinned to fail with the typed
//! error for a join-consumed handle instead of silently proceeding.

use std::future::pending;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nlos_process::ProcessAuthority;
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, FiberState, RuntimeAdapter, RuntimeError};
use nlos_runtime_tokio::{
    ChannelWaitError, ResumeRejection, SnapshotResumable, TokioRuntimeAdapter, TokioRuntimeConfig,
};
use nlos_types::{
    AgentInstanceId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ProcessId,
    ResourceGroupId, SchedulerDomainId,
};
use nlos_wait::BindingId;
use tokio::runtime::Handle;

fn id_bytes(value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[8..].copy_from_slice(&(value as u64).to_be_bytes());
    bytes
}

fn fiber_spec(index: usize, scope: CancellationScopeId) -> FiberSpec {
    FiberSpec {
        fiber_id: ExecutionFiberId::from_bytes(id_bytes(index)),
        fiber_generation: Generation::INITIAL,
        agent_instance_id: AgentInstanceId::from_bytes(id_bytes(index)),
        agent_generation: Generation::INITIAL,
        process_id: ProcessId::from_bytes(id_bytes(1)),
        process_generation: Generation::INITIAL,
        task_attempt_id: None,
        cancellation_scope_id: scope,
        cancellation_generation: Generation::INITIAL,
        resource_group_id: ResourceGroupId::from_bytes(id_bytes(1)),
        scheduler_domain_id: SchedulerDomainId::from_bytes(id_bytes(1)),
        deadline: None,
    }
}

fn config(max_live_fibers: usize, tombstone_capacity: usize) -> TokioRuntimeConfig {
    TokioRuntimeConfig {
        max_live_fibers,
        tombstone_capacity,
        ..TokioRuntimeConfig::default()
    }
}

fn runtime(max_live_fibers: usize, tombstone_capacity: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        config(max_live_fibers, tombstone_capacity),
    )
    .expect("runtime")
}

/// Scope-registry runtime: the fiber tombstone ring keeps its default while
/// `scope_tombstone_capacity` varies.
fn scope_runtime(max_live_fibers: usize, scope_tombstone_capacity: usize) -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        Handle::current(),
        TokioRuntimeConfig {
            max_live_fibers,
            scope_tombstone_capacity,
            ..TokioRuntimeConfig::default()
        },
    )
    .expect("runtime")
}

fn next_generation() -> Generation {
    Generation::INITIAL.checked_next().expect("next generation")
}

fn reaped(handle: FiberHandle) -> RuntimeError {
    RuntimeError::FiberReaped {
        fiber_id: handle.fiber_id,
        generation: handle.generation,
    }
}

async fn wait_for_state(runtime: &TokioRuntimeAdapter, handle: FiberHandle, expected: FiberState) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

/// Bounded poll on the registry size — the detach-at-terminal reaping is
/// observable only through the registry (the record is gone, so
/// `inspect`-based polling would race the reap).
async fn wait_for_registered(runtime: &TokioRuntimeAdapter, expected: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if runtime.registered_fibers() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("registry did not reach the expected size");
}

/// FIBER-REAP-001 / FIBER-REAP-002 — join 即回收:given a fiber that reached
/// its terminal state; when it is joined; then the record leaves the
/// registry in the same terminal critical section (the count drops) and a
/// second join of the same handle reports `FiberReaped` instead of
/// replaying the stored exit. Handle-addressed inspection of the reaped
/// generation reports the same typed error, not a bare stale-handle
/// rejection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn join_consumes_record_and_re_join_reports_fiber_reaped() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(40));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(
        runtime.registered_fibers(),
        1,
        "terminal but unconsumed record stays registered"
    );

    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(runtime.registered_fibers(), 0, "join must reap the record");

    assert_eq!(runtime.join_fiber(handle), Err(reaped(handle)));
    assert_eq!(runtime.inspect(handle), Err(reaped(handle)));
}

/// FIBER-REAP-003 (terminal branch) — detach 已终态立即回收:given a fiber
/// already in a terminal state; when it is detached; then the record is
/// reclaimed immediately (same terminal critical section as a consuming
/// join) and a later join reports `FiberReaped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_on_terminal_record_reaps_immediately() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(41));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    runtime.detach_fiber(handle).expect("detach terminal fiber");
    assert_eq!(
        runtime.registered_fibers(),
        0,
        "detach on a terminal record reaps immediately"
    );

    assert_eq!(runtime.join_fiber(handle), Err(reaped(handle)));
}

/// FIBER-REAP-003 (live branch) — detach 未终态终态后回收:given a live fiber
/// marked for reaping by `detach_fiber`; when it reaches its terminal
/// transition; then the terminal critical section reclaims the record and a
/// later join reports `FiberReaped`. While still live the record stays
/// registered (detach alone does not cancel or fence anything).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_on_live_fiber_reaps_at_terminal_transition() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(42));
    let handle = runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("spawn");

    runtime.detach_fiber(handle).expect("detach live fiber");
    assert_eq!(
        runtime.registered_fibers(),
        1,
        "a live detached fiber stays registered"
    );

    runtime
        .cancel_scope(scope, Generation::INITIAL)
        .expect("cancel");
    wait_for_registered(&runtime, 0).await;

    assert_eq!(runtime.join_fiber(handle), Err(reaped(handle)));
}

/// FIBER-REAP-004 — 墓碑窗口内防重:given a fiber consumed by a join (its
/// tombstone is inside the default-capacity ring); when the same
/// `(fiber_id, generation)` is re-spawned; then the spawn stays
/// `DuplicateFiber` — the window protection survives the record itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respawn_of_reaped_identity_is_fenced_within_tombstone_window() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(43));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));

    assert_eq!(
        runtime.spawn_fiber(fiber_spec(1, scope), Box::pin(pending())),
        Err(RuntimeError::DuplicateFiber),
        "a reaped (id, generation) stays fenced while its tombstone is in the ring"
    );
}

/// FIBER-REAP-004 (eviction) — 容量为 1 的环挤出后放行:with a
/// `tombstone_capacity = 1` ring; when a second identity is consumed; then
/// the first tombstone is FIFO-evicted and the evicted identity may spawn
/// again as a new fiber.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstone_ring_capacity_one_evicts_oldest_and_allows_respawn() {
    let runtime = runtime(4, 1);
    let scope = CancellationScopeId::from_bytes(id_bytes(44));

    let first = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn first");
    wait_for_state(&runtime, first, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(first), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.spawn_fiber(fiber_spec(1, scope), Box::pin(pending())),
        Err(RuntimeError::DuplicateFiber),
        "the single-slot ring still fences the reaped identity"
    );

    // Consuming a second identity evicts the first tombstone (FIFO).
    let second = runtime
        .spawn_fiber(
            fiber_spec(2, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn second");
    wait_for_state(&runtime, second, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(second), Ok(FiberExit::Completed));

    let respawned = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("evicted identity spawns as a new fiber");
    wait_for_state(&runtime, respawned, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(respawned), Ok(FiberExit::Completed));
}

/// §2.4 zero-value semantics — 容量为零的环:with `tombstone_capacity = 0`
/// the ring holds nothing; a consumed identity may re-spawn immediately
/// (pure consumption, no window protection) — explicitly legal, not a
/// disabled feature.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_tombstone_capacity_is_pure_consumption() {
    let runtime = runtime(4, 0);
    let scope = CancellationScopeId::from_bytes(id_bytes(45));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_fibers(),
        0,
        "the record is still consumed"
    );
    assert_eq!(
        runtime.join_fiber(handle),
        Err(RuntimeError::InvalidGeneration),
        "FiberReaped requires a tombstone hit; a zero-capacity ring degrades the re-join to the pre-existing unknown-handle error"
    );

    runtime
        .spawn_fiber(fiber_spec(1, scope), Box::pin(pending()))
        .expect("zero-capacity ring fences nothing");
}

// ---------------------------------------------------------------------------
// R1 pins (W25-001R): the FiberReaped externalization through the shared
// handle resolution. Minimal fixtures only — the reaped-handle gate fires
// before any durable interaction, so a bare process authority (never
// touched) and an stub snapshot suffice.
// ---------------------------------------------------------------------------

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
            "nlos-runtime-tokio-lifecycle-reap-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Snapshot-family stub: the values are never read — a reaped handle fails
/// the shared resolution before the process authority is touched.
struct StubSnapshot {
    binding_id: BindingId,
    process_id: ProcessId,
    incarnation: Generation,
}

impl SnapshotResumable for StubSnapshot {
    fn binding(&self) -> BindingId {
        self.binding_id
    }

    fn process_id(&self) -> ProcessId {
        self.process_id
    }

    fn expected_incarnation(&self) -> Generation {
        self.incarnation
    }

    fn handler_input(&self) -> Vec<u8> {
        b"entry".to_vec()
    }

    fn resume_from_entry(&self, _input: &[u8]) -> Result<(), ResumeRejection> {
        Ok(())
    }
}

/// R1 pin — `FiberReaped` 外延(Operation wait):given a join-consumed handle;
/// when an Operation wait is registered for it; then the registration fails
/// with `FiberReaped` — it must NOT resolve ready-`Cancelled` the way a
/// still-registered terminal fiber would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaped_handle_rejects_operation_wait_with_fiber_reaped() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(46));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));

    let operation = OperationId::from_bytes(id_bytes(61));
    let error = runtime
        .wait_for_operation(handle, operation, Generation::INITIAL)
        .err()
        .expect("a reaped generation must reject the Operation wait");
    assert_eq!(error, reaped(handle));
}

/// R1 pin — `FiberReaped` 外延(固有句柄方法):given a join-consumed handle;
/// when the inherent lifecycle inspection helpers resolve it; then they fail
/// with `FiberReaped` (one representative; the backpressure/suspend helpers
/// share the same resolution path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaped_handle_rejects_lifecycle_helpers_with_fiber_reaped() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(47));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));

    assert_eq!(runtime.inspect_lifecycle_phase(handle), Err(reaped(handle)));
    assert_eq!(runtime.begin_backpressure_wait(handle), Err(reaped(handle)));
}

/// R1 pin — `FiberReaped` 外延(snapshot 族 GC):given a join-consumed handle;
/// when the durable snapshot-family GC is invoked for it; then it fails with
/// `ChannelWaitError::Runtime(FiberReaped)` — unlike a still-registered
/// terminal fiber (which the GC entry deliberately serves), a reaped
/// generation no longer authorizes any durable side effect.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reaped_handle_rejects_snapshot_gc_with_runtime_fiber_reaped() {
    let root = Root::new("reap-gc");
    let process = ProcessAuthority::open(root.path()).expect("open process authority");
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(48));
    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");

    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));

    let snapshot = StubSnapshot {
        binding_id: BindingId::from_bytes(id_bytes(62)),
        process_id: ProcessId::from_bytes(id_bytes(1)),
        incarnation: Generation::INITIAL,
    };
    let error = runtime
        .gc_handler_entry_snapshot(handle, &process, &snapshot)
        .expect_err("a reaped generation must fail the snapshot-family GC");
    match error {
        ChannelWaitError::Runtime(RuntimeError::FiberReaped {
            fiber_id,
            generation,
        }) => {
            assert_eq!(fiber_id, handle.fiber_id);
            assert_eq!(generation, handle.generation);
        }
        other => panic!("expected Runtime(FiberReaped), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Scope registry (SCOPE-IDX-001..004): by-id index, reference-count reaping,
// and the bounded scope tombstone ring. Observation surface:
// `registered_scopes()` counts live scope entries — an entry stays
// registered while at least one unreaped fiber record (or an in-flight
// admission) references it, and persists only as a tombstone afterwards.
// ---------------------------------------------------------------------------

/// SCOPE-IDX-001 / SCOPE-IDX-004 — 乱序注册/查询/取消的索引正确性:given
/// scopes registered under scrambled ids with one live fiber each; when they
/// are cancelled in reverse registration order and generation fences are
/// probed; then the by-id index resolves exactly the targeted scope — the
/// cancelled fiber terminates, every other fiber keeps running, a mismatched
/// generation on a live id is `InvalidGeneration`, and the exact
/// `(id, generation)` keeps answering cancels while unreaped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn out_of_order_scope_ids_index_query_and_cancel_correctly() {
    let runtime = runtime(16, 65_536);
    // Ids whose hash-map bucket order differs from the registration order.
    let ids: Vec<CancellationScopeId> = (0..8)
        .map(|index| CancellationScopeId::from_bytes(id_bytes(9_000 + (index * 37) % 101)))
        .collect();
    let handles: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(index, scope)| {
            runtime
                .spawn_fiber(fiber_spec(100 + index, *scope), Box::pin(pending()))
                .expect("spawn")
        })
        .collect();
    assert_eq!(
        runtime.registered_scopes(),
        8,
        "one live entry per distinct scope id"
    );

    for (slot, scope) in ids.iter().enumerate().rev() {
        runtime
            .cancel_scope(*scope, Generation::INITIAL)
            .expect("cancel");
        wait_for_state(&runtime, handles[slot], FiberState::Cancelled).await;
    }
    assert_eq!(
        runtime.registered_scopes(),
        8,
        "cancelled scopes stay registered while their fibers are unreaped"
    );

    let mut bumped = fiber_spec(200, ids[3]);
    bumped.cancellation_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "a live scope id is locked to its first generation"
    );
    assert_eq!(
        runtime.cancel_scope(ids[3], Generation::INITIAL),
        Ok(()),
        "the exact (id, generation) keeps resolving through the index"
    );
}

/// SCOPE-IDX-002 — 最后 fiber 回收后条目消失:given a scope shared by two
/// fibers and a neighboring single-fiber scope; when fibers are joined one
/// by one; then the shared entry survives until its LAST record is reaped,
/// the neighbor is unaffected, and a cancel against the reaped scope fails
/// closed with `InvalidGeneration` instead of panicking or succeeding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_entry_is_reaped_after_its_last_fiber_record() {
    let runtime = runtime(8, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(60));
    let neighbor = CancellationScopeId::from_bytes(id_bytes(61));

    let first = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn first");
    let second = runtime
        .spawn_fiber(
            fiber_spec(2, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn second");
    let bystander = runtime
        .spawn_fiber(
            fiber_spec(3, neighbor),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn bystander");
    assert_eq!(runtime.registered_scopes(), 2);

    wait_for_state(&runtime, first, FiberState::Completed).await;
    wait_for_state(&runtime, second, FiberState::Completed).await;
    assert_eq!(
        runtime.registered_scopes(),
        2,
        "terminal-but-unreaped records keep their scope registered"
    );

    assert_eq!(runtime.join_fiber(first), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_scopes(),
        2,
        "one unreaped record still references the shared scope"
    );

    assert_eq!(runtime.join_fiber(second), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_scopes(),
        1,
        "the last reap removes the entry; the neighbor is unaffected"
    );

    assert_eq!(
        runtime.cancel_scope(scope, Generation::INITIAL),
        Err(RuntimeError::InvalidGeneration),
        "a reaped scope fails cancellation closed"
    );

    assert_eq!(runtime.join_fiber(bystander), Ok(FiberExit::Completed));
    assert_eq!(runtime.registered_scopes(), 0);
}

/// SCOPE-IDX-002 (balancing) — 失败准入的引用归还:given a reaped scope id
/// whose only outstanding reference is a spawn attempt that then fails the
/// fiber tombstone fence; when the attempt is rejected; then the acquisition
/// is balanced out and no scope entry leaks. This also pins the window
/// semantics: the reaped `(id, generation)` itself re-registers a fresh
/// scope — only a mismatched generation is fenced (SCOPE-IDX-003/004).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_admission_balances_its_scope_reference() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(62));

    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_scopes(),
        0,
        "the scope was reaped with its record"
    );

    // The scope stage passes (same generation as the tombstone) but the
    // fiber identity fence rejects the re-spawn.
    assert_eq!(
        runtime.spawn_fiber(fiber_spec(1, scope), Box::pin(pending())),
        Err(RuntimeError::DuplicateFiber)
    );
    assert_eq!(
        runtime.registered_scopes(),
        0,
        "a rejected admission must not leave a scope entry behind"
    );
}

/// SCOPE-IDX-003 / SCOPE-IDX-004 — 窗口内同 id 的代次锁:given a scope whose
/// last fiber was reaped (its `(id, generation)` entered the tombstone
/// ring); when a fresh fiber registers the same id under a different
/// generation; then the registration stays `InvalidGeneration` — the
/// tombstone extends the first-generation lock past the entry's lifetime —
/// while a different id registers freely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstoned_scope_id_fences_mismatched_generation_within_window() {
    let runtime = runtime(4, 65_536);
    let scope = CancellationScopeId::from_bytes(id_bytes(63));
    let other = CancellationScopeId::from_bytes(id_bytes(64));

    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_scopes(),
        0,
        "the scope id now exists only as a tombstone"
    );

    let mut bumped = fiber_spec(2, scope);
    bumped.cancellation_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "the tombstone keeps the id locked to its reaped generation"
    );

    let control = runtime
        .spawn_fiber(
            fiber_spec(3, other),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("a different id registers freely");
    wait_for_state(&runtime, control, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(control), Ok(FiberExit::Completed));
    assert_eq!(runtime.registered_scopes(), 0);
}

/// SCOPE-IDX-003 (eviction) — 容量为 1 的 scope 墓碑环挤出后放行:with
/// `scope_tombstone_capacity = 1`; when a second scope id is reaped; then
/// the first id's tombstone is FIFO-evicted and the id accepts a fresh
/// registration under a new generation — the same registration that was
/// `InvalidGeneration` inside the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_tombstone_ring_capacity_one_evicts_and_allows_new_generation() {
    let runtime = scope_runtime(8, 1);
    let first = CancellationScopeId::from_bytes(id_bytes(65));
    let second = CancellationScopeId::from_bytes(id_bytes(66));

    let a = runtime
        .spawn_fiber(
            fiber_spec(1, first),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn first-scope fiber");
    wait_for_state(&runtime, a, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(a), Ok(FiberExit::Completed));

    let mut bumped = fiber_spec(2, first);
    bumped.cancellation_generation = next_generation();
    assert_eq!(
        runtime.spawn_fiber(bumped, Box::pin(pending())),
        Err(RuntimeError::InvalidGeneration),
        "within the window the mismatched generation stays fenced"
    );

    // Reaping a second scope evicts the first tombstone (FIFO, capacity 1).
    let b = runtime
        .spawn_fiber(
            fiber_spec(3, second),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn second-scope fiber");
    wait_for_state(&runtime, b, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(b), Ok(FiberExit::Completed));

    let mut evicted = fiber_spec(4, first);
    evicted.cancellation_generation = next_generation();
    let respawned = runtime
        .spawn_fiber(evicted, Box::pin(async { FiberExit::Completed }))
        .expect("an evicted scope id accepts a new generation");
    wait_for_state(&runtime, respawned, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(respawned), Ok(FiberExit::Completed));
    assert_eq!(runtime.registered_scopes(), 0);
}

/// §2.4 zero-value semantics (scope ring) — 容量为零的 scope 墓碑环:with
/// `scope_tombstone_capacity = 0` a reaped scope id fences nothing — a
/// different generation may register immediately (pure consumption, no
/// window protection); the entry itself is still reaped with its record.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_scope_tombstone_capacity_fences_nothing() {
    let runtime = scope_runtime(4, 0);
    let scope = CancellationScopeId::from_bytes(id_bytes(67));

    let handle = runtime
        .spawn_fiber(
            fiber_spec(1, scope),
            Box::pin(async { FiberExit::Completed }),
        )
        .expect("spawn");
    wait_for_state(&runtime, handle, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(handle), Ok(FiberExit::Completed));
    assert_eq!(
        runtime.registered_scopes(),
        0,
        "the entry is still reaped with its record"
    );

    let mut bumped = fiber_spec(2, scope);
    bumped.cancellation_generation = next_generation();
    let respawned = runtime
        .spawn_fiber(bumped, Box::pin(async { FiberExit::Completed }))
        .expect("a zero-capacity ring fences nothing");
    wait_for_state(&runtime, respawned, FiberState::Completed).await;
    assert_eq!(runtime.join_fiber(respawned), Ok(FiberExit::Completed));
    assert_eq!(runtime.registered_scopes(), 0);
}
