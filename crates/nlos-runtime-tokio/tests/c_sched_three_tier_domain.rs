//! Stage C (`C-SCHED` / `W64-CSCHED`) conformance and integration tests for
//! the three-tier `Global -> Cell -> Worker` `SchedulerDomain`, bounded
//! per-`ResourceGroup` WFQ/aging queues, safe work stealing, `Context`
//! affinity routing, and cross-`Cell` reverse backpressure (`§25.2.2`).

use std::collections::BTreeSet;
use std::sync::Arc;

use nlos_runtime::{
    AdmissionDirective, ContextAffinitySpec, DeadlineClass, DomainPressureLevel,
    ResourceGroupQueuePolicy, SaturationSource, SchedulerAdmission, SchedulerDomainId,
    SchedulerTier, StealRejectionReason, ThrottleReason, WorkStealEnvelope,
    WorkerStealCapabilities,
};
use nlos_runtime_tokio::{
    ScheduledWorkItem, StealError, ThreeTierScheduler, WorkerDomainConfig, WorkerSchedulerDomain,
};
use nlos_types::ExecutionFiberId;

const GLOBAL_ID: [u8; 16] = [0xA0; 16];
const CELL_A_ID: [u8; 16] = [0xB1; 16];
const CELL_B_ID: [u8; 16] = [0xB2; 16];
const WORKER_1_ID: [u8; 16] = [0xC1; 16];
const WORKER_2_ID: [u8; 16] = [0xC2; 16];
const WORKER_3_ID: [u8; 16] = [0xC3; 16];
const WORKER_4_ID: [u8; 16] = [0xC4; 16];

const RG_ALPHA: [u8; 16] = [0xD1; 16];
const RG_BETA: [u8; 16] = [0xD2; 16];

const NS_ROOT: [u8; 32] = [0x11; 32];
const SECRET_SCOPE: [u8; 16] = [0x22; 16];
const CTX_WARM_A: [u8; 32] = [0x33; 32];
const DEVICE_GPU_0: [u8; 16] = [0x44; 16];
const TENANT_MAIN: [u8; 16] = [0x55; 16];
const AUTHORITY_MAIN: [u8; 16] = [0x66; 16];
const ISOLATION_MAIN: [u8; 16] = [0x77; 16];

fn default_capabilities() -> WorkerStealCapabilities {
    WorkerStealCapabilities {
        allowed_namespaces: vec![NS_ROOT],
        allowed_secret_scopes: vec![SECRET_SCOPE],
        attached_devices: vec![DEVICE_GPU_0],
        allowed_provider_tenants: vec![TENANT_MAIN],
        trusted_task_authorities: vec![(AUTHORITY_MAIN, 1)],
        active_isolation_domains: vec![ISOLATION_MAIN],
        fenced_isolation_domains: Vec::new(),
        warm_context_digests: Vec::new(),
        require_warm_context_on_steal: false,
    }
}

fn default_envelope() -> WorkStealEnvelope {
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

fn make_item(
    seed: u8,
    resource_group_id: [u8; 16],
    requested_priority: u8,
    deadline_class: DeadlineClass,
    enqueued_at_ms: u64,
) -> ScheduledWorkItem {
    let mut id = [0u8; 16];
    id[0] = seed;
    id[15] = seed.wrapping_mul(3);
    ScheduledWorkItem {
        item_id: id,
        fiber_id: Some(ExecutionFiberId::from_bytes(id)),
        resource_group_id,
        requested_priority,
        deadline_class,
        cancel_epoch: 1,
        enqueued_at_ms,
        runnable: true,
        steal_envelope: default_envelope(),
    }
}

fn default_rg_policy(group_id: [u8; 16]) -> ResourceGroupQueuePolicy {
    ResourceGroupQueuePolicy {
        resource_group_id: group_id,
        max_queue_depth: 64,
        parent_hard_quota_in_flight: 32,
        weight: 1,
        max_priority_cap: 200,
        starvation_aging_step_ms: 50,
    }
}

#[test]
fn sched_hier_001_domain_id_lineage_and_tier_contracts() {
    let global = SchedulerDomainId::global(GLOBAL_ID);
    let cell_a = SchedulerDomainId::cell(GLOBAL_ID, CELL_A_ID);
    let cell_b = SchedulerDomainId::cell([0xFF; 16], CELL_B_ID);
    let worker_1 = SchedulerDomainId::worker(CELL_A_ID, WORKER_1_ID);

    assert_eq!(global.tier(), SchedulerTier::Global);
    assert!(global.is_global());
    assert!(!global.is_cell());
    assert!(!global.is_worker());
    assert_eq!(global.global_id(), Some(GLOBAL_ID));
    assert_eq!(global.cell_id(), None);
    assert_eq!(global.worker_id(), None);

    assert_eq!(cell_a.tier(), SchedulerTier::Cell);
    assert!(cell_a.is_cell());
    assert!(cell_a.belongs_to_global(GLOBAL_ID));
    assert!(cell_a.belongs_to_cell(CELL_A_ID));
    assert!(!cell_b.belongs_to_global(GLOBAL_ID));

    assert_eq!(worker_1.tier(), SchedulerTier::Worker);
    assert!(worker_1.is_worker());
    assert!(worker_1.belongs_to_cell(CELL_A_ID));
    assert!(!worker_1.belongs_to_cell(CELL_B_ID));
    assert!(!worker_1.belongs_to_global(GLOBAL_ID));
    assert!(worker_1.belongs_to_global_via(GLOBAL_ID, &cell_a));
    assert!(!worker_1.belongs_to_global_via(GLOBAL_ID, &cell_b));

    assert_eq!(worker_1.leaf_nominal_id().into_bytes(), WORKER_1_ID);
    assert_eq!(
        default_envelope().context_affinity(),
        ContextAffinitySpec::none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sched_hier_001_concurrent_multi_worker_tokio_dispatch_without_global_lock_contention() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 256);
    let cell = scheduler.add_cell(CELL_A_ID, 256);
    cell.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 128,
        parent_hard_quota_in_flight: 128,
        weight: 2,
        max_priority_cap: 200,
        starvation_aging_step_ms: 100,
    });

    let worker_ids = [WORKER_1_ID, WORKER_2_ID, WORKER_3_ID, WORKER_4_ID];
    let mut workers = Vec::new();
    for wid in worker_ids {
        workers.push(cell.add_worker(
            wid,
            WorkerDomainConfig {
                max_concurrency: 32,
                base_materialization_window: 64,
            },
            default_capabilities(),
        ));
    }

    let global_locks_before = scheduler.global().control_lock_acquisitions();
    let cell_locks_before = cell.placement_lock_acquisitions();

    let items_per_worker = 40u8;
    let mut handles = Vec::new();

    for (w_idx, worker) in workers.iter().cloned().enumerate() {
        let base_seed = u8::try_from(w_idx * usize::from(items_per_worker)).expect("seed fits u8");
        handles.push(tokio::spawn(async move {
            let mut dispatched_ids = Vec::new();
            for offset in 0..items_per_worker {
                let mut item = make_item(
                    base_seed.wrapping_add(offset),
                    RG_ALPHA,
                    80,
                    DeadlineClass::Interactive,
                    1_000,
                );
                // Half start parked and transition via local `wake_item`.
                let starts_parked = offset % 2 == 1;
                item.runnable = !starts_parked;
                let id = item.item_id;

                let adm = worker.enqueue(item, 1_000);
                assert!(adm.is_admitted());
                if starts_parked {
                    assert!(worker.wake_item(id));
                }

                worker.wait_for_runnable().await;
                let popped = worker.dequeue_next(1_010).expect("local runnable item");
                dispatched_ids.push(popped.item.item_id);
                worker.complete_dispatched(&popped);
            }
            dispatched_ids
        }));
    }

    let mut all_dispatched = BTreeSet::new();
    for handle in handles {
        let ids = handle.await.expect("worker task succeeded");
        assert_eq!(ids.len(), usize::from(items_per_worker));
        for id in ids {
            assert!(all_dispatched.insert(id), "duplicate dispatch detected");
        }
    }

    assert_eq!(all_dispatched.len(), 4 * usize::from(items_per_worker));
    // [SCHED-HIER-001]: Local Worker enqueue, wake, and dispatch acquired ZERO
    // Global control locks and ZERO Cell placement locks.
    assert_eq!(
        scheduler.global().control_lock_acquisitions(),
        global_locks_before
    );
    assert_eq!(cell.placement_lock_acquisitions(), cell_locks_before);

    for worker in &workers {
        assert_eq!(worker.local_enqueues(), u64::from(items_per_worker));
        assert_eq!(worker.local_dispatches(), u64::from(items_per_worker));
        assert_eq!(worker.local_wakes(), u64::from(items_per_worker / 2));
        assert_eq!(worker.in_flight_count(), 0);
    }
}

#[test]
fn sched_queue_001_bounded_resource_group_queue_and_drr_weighted_fair_share() {
    let worker = WorkerSchedulerDomain::new(
        CELL_A_ID,
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );

    worker.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 6,
        parent_hard_quota_in_flight: 16,
        weight: 3,
        max_priority_cap: 200,
        starvation_aging_step_ms: 0,
    });
    worker.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_BETA,
        max_queue_depth: 4,
        parent_hard_quota_in_flight: 16,
        weight: 1,
        max_priority_cap: 200,
        starvation_aging_step_ms: 0,
    });

    for i in 0..6u8 {
        assert!(
            worker
                .enqueue(make_item(i, RG_ALPHA, 100, DeadlineClass::Standard, 0), 0)
                .is_admitted()
        );
    }
    // 7th item into RG_ALPHA exceeds bounded `max_queue_depth == 6`.
    assert_eq!(
        worker.enqueue(make_item(99, RG_ALPHA, 100, DeadlineClass::Standard, 0), 0),
        SchedulerAdmission::RejectedQueueFull {
            domain: worker.domain_id(),
            resource_group_id: RG_ALPHA,
            max_queue_depth: 6,
        }
    );

    for i in 10..14u8 {
        assert!(
            worker
                .enqueue(make_item(i, RG_BETA, 100, DeadlineClass::Standard, 0), 0)
                .is_admitted()
        );
    }

    // Over 8 dispatches, weight 3:1 DRR yields 6 from RG_ALPHA and 2 from RG_BETA.
    let mut alpha_count = 0usize;
    let mut beta_count = 0usize;
    for _ in 0..8 {
        let dispatched = worker.dequeue_next(10).expect("item available");
        if dispatched.item.resource_group_id == RG_ALPHA {
            alpha_count += 1;
        } else {
            beta_count += 1;
        }
        worker.complete_dispatched(&dispatched);
    }
    assert_eq!(alpha_count, 6);
    assert_eq!(beta_count, 2);
}

#[test]
fn sched_queue_001_priority_clamping_starvation_aging_and_full_queue_inspection() {
    let worker = WorkerSchedulerDomain::new(
        CELL_A_ID,
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );

    worker.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 16,
        parent_hard_quota_in_flight: 8,
        weight: 4,
        max_priority_cap: 150,
        starvation_aging_step_ms: 100,
    });

    // Item 1: BackgroundBatch requested at 40 at t=0.
    let aged_batch = make_item(1, RG_ALPHA, 40, DeadlineClass::BackgroundBatch, 0);
    let batch_id = aged_batch.item_id;
    assert!(worker.enqueue(aged_batch, 0).is_admitted());

    // Item 2: Caller requests 240 (> max_priority_cap 150) at t=5_000 -> clamped to 150.
    let clamped_item = make_item(2, RG_ALPHA, 240, DeadlineClass::Interactive, 5_000);
    let clamped_id = clamped_item.item_id;
    let adm = worker.enqueue(clamped_item, 5_000);
    assert_eq!(
        adm,
        SchedulerAdmission::Admitted {
            assigned_domain: worker.domain_id(),
            effective_priority: 150,
            context_affinity_hit: false,
            materialization_window_limit: 16,
        }
    );

    // Item 3: Standard priority 90 freshly enqueued at t=7_000.
    let fresh_standard = make_item(3, RG_ALPHA, 90, DeadlineClass::Standard, 7_000);
    let fresh_id = fresh_standard.item_id;
    assert!(worker.enqueue(fresh_standard, 7_000).is_admitted());

    // At t=7_000: `aged_batch` has waited 7_000ms -> +70 boost => effective_priority 110!
    // That promotes `aged_batch` (110) ahead of `fresh_standard` (90), right behind `clamped_item` (150).
    let inspection = worker
        .inspect_queue_item(batch_id, 7_000)
        .expect("batch item queued");
    assert_eq!(inspection.requested_priority, 40);
    assert_eq!(inspection.effective_priority, 110);
    assert_eq!(inspection.deadline_class, DeadlineClass::BackgroundBatch);
    assert_eq!(inspection.weight, 4);
    assert_eq!(inspection.queue_position_range, (1, 1));
    assert_eq!(inspection.throttle_reason, None);
    assert_eq!(inspection.starvation_age_ms, 7_000);
    assert_eq!(inspection.resource_group_id, RG_ALPHA);
    assert_eq!(inspection.cancel_epoch, 1);

    let first = worker.dequeue_next(7_000).expect("first");
    let second = worker.dequeue_next(7_000).expect("second");
    let third = worker.dequeue_next(7_000).expect("third");
    assert_eq!(first.item.item_id, clamped_id);
    assert_eq!(second.item.item_id, batch_id);
    assert_eq!(second.effective_priority, 110);
    assert_eq!(third.item.item_id, fresh_id);
}

#[test]
fn sched_queue_001_parent_hard_quota_and_cancellation_epoch_fence_across_cell_workers() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 32);
    let cell = scheduler.add_cell(CELL_A_ID, 32);
    cell.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 16,
        parent_hard_quota_in_flight: 2,
        weight: 2,
        max_priority_cap: 200,
        starvation_aging_step_ms: 100,
    });

    let w1 = cell.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );
    let w2 = cell.add_worker(
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );

    let item_a = make_item(10, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    let item_b = make_item(11, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    let mut item_c = make_item(12, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    item_c.cancel_epoch = 3;
    let mut item_surviving = make_item(13, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    item_surviving.cancel_epoch = 5;

    assert!(w1.enqueue(item_a, 0).is_admitted());
    assert!(w2.enqueue(item_b, 0).is_admitted());
    assert!(w2.enqueue(item_c, 0).is_admitted());
    assert!(w1.enqueue(item_surviving, 0).is_admitted());

    // Dispatch 1 on w1 and 1 on w2 -> Cell-wide parent_hard_quota_in_flight (2) reached!
    let d1 = w1.dequeue_next(10).expect("d1");
    let d2 = w2.dequeue_next(10).expect("d2");

    // 3rd dispatch across either Worker is blocked by parent hard quota.
    assert!(w1.dequeue_next(10).is_none());
    assert!(w2.dequeue_next(10).is_none());

    let throttled_inspect = w2
        .inspect_queue_item(item_c.item_id, 10)
        .expect("item_c still queued");
    assert_eq!(
        throttled_inspect.throttle_reason,
        Some(ThrottleReason::ParentHardQuotaReached)
    );

    // Advance Cell-wide cancellation fence to epoch 4: purges `item_c` (epoch 3),
    // keeps `item_surviving` (epoch 5), and rejects stale enqueue at epoch 4.
    let purged = cell.advance_cancel_epoch(RG_ALPHA, 4);
    assert_eq!(purged, 1);
    assert!(w2.inspect_queue_item(item_c.item_id, 20).is_none());

    let mut stale = make_item(14, RG_ALPHA, 100, DeadlineClass::Standard, 20);
    stale.cancel_epoch = 4;
    assert_eq!(
        w1.enqueue(stale, 20),
        SchedulerAdmission::RejectedCancelled {
            cancel_epoch: 4,
            fenced_epoch: 4,
        }
    );

    // Release one in-flight slot -> `item_surviving` (epoch 5) can now dispatch.
    w1.complete_dispatched(&d1);
    let d3 = w1.dequeue_next(25).expect("surviving item dispatches");
    assert_eq!(d3.item.item_id, item_surviving.item_id);
    w2.complete_dispatched(&d2);
    w1.complete_dispatched(&d3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sched_steal_001_work_stealing_transfers_ownership_atomically_with_zero_duplication() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 128);
    let cell = scheduler.add_cell(CELL_A_ID, 128);
    cell.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 128,
        parent_hard_quota_in_flight: 64,
        weight: 1,
        max_priority_cap: 200,
        starvation_aging_step_ms: 50,
    });

    let workers: Vec<Arc<WorkerSchedulerDomain>> =
        [WORKER_1_ID, WORKER_2_ID, WORKER_3_ID, WORKER_4_ID]
            .into_iter()
            .map(|wid| {
                cell.add_worker(
                    wid,
                    WorkerDomainConfig {
                        max_concurrency: 32,
                        base_materialization_window: 32,
                    },
                    default_capabilities(),
                )
            })
            .collect();

    // Overload Worker 1 with 40 migratable items while Worker 1's concurrent loop
    // performs at most 12 local dispatches, guaranteeing concurrent peer steals.
    let total_items = 40u8;
    for i in 0..total_items {
        assert!(
            workers[0]
                .enqueue(make_item(i, RG_ALPHA, 80, DeadlineClass::Standard, 0), 0)
                .is_admitted()
        );
    }

    let barrier = Arc::new(tokio::sync::Barrier::new(4));
    let mut handles = Vec::new();
    for (idx, worker) in workers.iter().enumerate() {
        let w = Arc::clone(worker);
        let c = Arc::clone(&cell);
        let b = Arc::clone(&barrier);
        let max_iters = if idx == 0 { 12u64 } else { 32u64 };
        handles.push(tokio::spawn(async move {
            b.wait().await;
            let mut local_executed = Vec::new();
            for step in 0..max_iters {
                if let Some(dispatched) = w.dequeue_next(step) {
                    local_executed.push(dispatched.item.item_id);
                    tokio::task::yield_now().await;
                    w.complete_dispatched(&dispatched);
                } else {
                    let report = c
                        .steal_from_overloaded_peers(w.worker_id(), step)
                        .expect("intra-cell steal");
                    if report.stolen_item_id.is_none() {
                        tokio::task::yield_now().await;
                    }
                }
            }
            local_executed
        }));
    }

    let mut executed = Vec::new();
    for handle in handles {
        executed.extend(handle.await.expect("worker task"));
    }
    // Drain any stragglers if a late steal occurred on the final iteration.
    for w in &workers {
        while let Some(d) = w.dequeue_next(100) {
            executed.push(d.item.item_id);
            w.complete_dispatched(&d);
        }
    }

    let unique: BTreeSet<[u8; 16]> = executed.iter().copied().collect();
    assert_eq!(executed.len(), usize::from(total_items));
    assert_eq!(unique.len(), usize::from(total_items));
    assert!(workers[0].stolen_out_count() > 0);
}

#[test]
fn sched_steal_001_strictly_refuses_active_effect_permit_reservation_and_device_lease() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 32);
    let cell = scheduler.add_cell(CELL_A_ID, 32);
    cell.register_resource_group(default_rg_policy(RG_ALPHA));

    let victim = cell.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );
    let thief = cell.add_worker(
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );

    let mut non_migratable = make_item(1, RG_ALPHA, 150, DeadlineClass::Interactive, 0);
    non_migratable.steal_envelope.migratable = false;

    let mut active_effect = make_item(2, RG_ALPHA, 150, DeadlineClass::Interactive, 0);
    active_effect.steal_envelope.has_active_effect_permit = true;

    let mut active_reservation = make_item(3, RG_ALPHA, 150, DeadlineClass::Interactive, 0);
    active_reservation.steal_envelope.has_active_reservation = true;

    let mut active_lease = make_item(4, RG_ALPHA, 150, DeadlineClass::Interactive, 0);
    active_lease.steal_envelope.has_active_device_lease = true;

    let clean_stealable = make_item(5, RG_ALPHA, 60, DeadlineClass::Standard, 0);
    let clean_id = clean_stealable.item_id;

    for item in [
        non_migratable,
        active_effect,
        active_reservation,
        active_lease,
        clean_stealable,
    ] {
        assert!(victim.enqueue(item, 0).is_admitted());
    }

    let report = thief
        .steal_from_peer_worker(&victim, 10)
        .expect("steal attempt");
    assert_eq!(report.stolen_item_id, Some(clean_id));
    assert_eq!(report.skipped_ineligible.len(), 4);

    let reasons: Vec<StealRejectionReason> = report
        .skipped_ineligible
        .iter()
        .map(|(_, reason)| *reason)
        .collect();
    assert!(reasons.contains(&StealRejectionReason::NotMigratable));
    assert!(reasons.contains(&StealRejectionReason::ActiveEffectPermit));
    assert!(reasons.contains(&StealRejectionReason::ActiveReservation));
    assert!(reasons.contains(&StealRejectionReason::ActiveExclusiveDeviceLease));

    // All 4 non-stealable items remain intact on `victim`; `thief` holds only `clean_id`.
    assert_eq!(victim.runnable_queue_len(), 4);
    assert_eq!(thief.runnable_queue_len(), 1);
    let popped_on_thief = thief.dequeue_next(15).expect("stolen item on thief");
    assert_eq!(popped_on_thief.item.item_id, clean_id);
    assert!(thief.dequeue_next(15).is_none());

    // Subsequent steal finds zero eligible items and leaves victim intact.
    let second_report = thief
        .steal_from_peer_worker(&victim, 20)
        .expect("second steal");
    assert_eq!(second_report.stolen_item_id, None);
    assert_eq!(victim.runnable_queue_len(), 4);
}

#[test]
fn sched_steal_001_enforces_fenced_isolation_namespace_secret_tenant_authority_and_device_affinity()
{
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 32);
    let cell = scheduler.add_cell(CELL_A_ID, 32);
    cell.register_resource_group(default_rg_policy(RG_ALPHA));

    // Victim has broad capabilities; thief only supports the baseline capability set.
    let mut broad_caps = default_capabilities();
    broad_caps.allowed_namespaces.push([0x91; 32]);
    broad_caps.allowed_secret_scopes.push([0x92; 16]);
    broad_caps.attached_devices.push([0x93; 16]);
    broad_caps.allowed_provider_tenants.push([0x94; 16]);
    broad_caps.active_isolation_domains.push([0x95; 16]);

    let victim = cell.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        broad_caps,
    );

    let mut thief_caps = default_capabilities();
    thief_caps.trusted_task_authorities = vec![(AUTHORITY_MAIN, 5)];
    let thief = cell.add_worker(
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        thief_caps,
    );

    let mut wrong_ns = make_item(21, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    wrong_ns.steal_envelope.namespace_root = [0x91; 32];
    wrong_ns.steal_envelope.authority_term = 5;

    let mut wrong_secret = make_item(22, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    wrong_secret.steal_envelope.secret_scope_id = Some([0x92; 16]);
    wrong_secret.steal_envelope.authority_term = 5;

    let mut wrong_device = make_item(23, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    wrong_device.steal_envelope.device_affinity = Some([0x93; 16]);
    wrong_device.steal_envelope.authority_term = 5;

    let mut wrong_tenant = make_item(24, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    wrong_tenant.steal_envelope.provider_tenant_id = Some([0x94; 16]);
    wrong_tenant.steal_envelope.authority_term = 5;

    let mut stale_authority = make_item(25, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    stale_authority.steal_envelope.authority_term = 4; // Thief requires term >= 5.

    let mut fenced_iso_flag = make_item(26, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    fenced_iso_flag.steal_envelope.isolation_fenced = true;
    fenced_iso_flag.steal_envelope.authority_term = 5;

    let mut dynamic_iso_item = make_item(27, RG_ALPHA, 100, DeadlineClass::Standard, 0);
    dynamic_iso_item.steal_envelope.authority_term = 5;

    for item in [
        wrong_ns,
        wrong_secret,
        wrong_device,
        wrong_tenant,
        stale_authority,
        fenced_iso_flag,
        dynamic_iso_item,
    ] {
        assert!(victim.enqueue(item, 0).is_admitted());
    }

    // Dynamically fence `ISOLATION_MAIN` on `thief` before stealing!
    thief.fence_isolation_domain(ISOLATION_MAIN);

    let report = thief
        .steal_from_peer_worker(&victim, 10)
        .expect("steal attempt");
    assert_eq!(report.stolen_item_id, None);
    assert_eq!(victim.runnable_queue_len(), 7);
    assert_eq!(report.skipped_ineligible.len(), 7);

    // Build a second healthy thief where `ISOLATION_MAIN` is NOT fenced to verify
    // each individual mismatch rejection reason deterministically.
    let mut healthy_peer_caps = default_capabilities();
    healthy_peer_caps.trusted_task_authorities = vec![(AUTHORITY_MAIN, 5)];
    let healthy_peer = cell.add_worker(
        WORKER_3_ID,
        WorkerDomainConfig {
            max_concurrency: 16,
            base_materialization_window: 16,
        },
        healthy_peer_caps,
    );

    let peer_report = healthy_peer
        .steal_from_peer_worker(&victim, 15)
        .expect("healthy_peer steal");
    // Only `dynamic_iso_item` satisfies all of `healthy_peer`'s gates; the other 6 are rejected!
    assert_eq!(peer_report.stolen_item_id, Some(dynamic_iso_item.item_id));
    let rejected_reasons: Vec<StealRejectionReason> = peer_report
        .skipped_ineligible
        .iter()
        .map(|(_, reason)| *reason)
        .collect();
    assert!(rejected_reasons.contains(&StealRejectionReason::NamespaceMismatch));
    assert!(rejected_reasons.contains(&StealRejectionReason::SecretScopeMismatch));
    assert!(rejected_reasons.contains(&StealRejectionReason::DeviceAffinityUnsatisfied));
    assert!(rejected_reasons.contains(&StealRejectionReason::ProviderTenantMismatch));
    assert!(rejected_reasons.contains(&StealRejectionReason::TaskAuthorityMismatch));
    assert!(rejected_reasons.contains(&StealRejectionReason::IsolationDomainFenced));
}

#[test]
fn context_affinity_placement_hits_warm_worker_cache_until_saturated_then_falls_back() {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 32);
    let cell = scheduler.add_cell(CELL_A_ID, 32);
    cell.register_resource_group(ResourceGroupQueuePolicy {
        resource_group_id: RG_ALPHA,
        max_queue_depth: 5,
        parent_hard_quota_in_flight: 16,
        weight: 1,
        max_priority_cap: 200,
        starvation_aging_step_ms: 50,
    });

    let w1 = cell.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );
    let w2 = cell.add_worker(
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );
    // Warm `CTX_WARM_A` exclusively on Worker 2.
    w2.register_warm_context(CTX_WARM_A);

    for i in 0..4u8 {
        let mut item = make_item(i, RG_ALPHA, 100, DeadlineClass::Interactive, 0);
        item.steal_envelope.preferred_context_digest = Some(CTX_WARM_A);
        let adm = cell.place_runnable(item, 0);
        // After the 3rd enqueue (`i == 2`), `w2` reaches 3/5 = 60% (`Elevated`),
        // contracting the Cell window from 32 to 24 prior to `i == 3`.
        let expected_window = if i < 3 { 32 } else { 24 };
        assert_eq!(
            adm,
            SchedulerAdmission::Admitted {
                assigned_domain: w2.domain_id(),
                effective_priority: 100,
                context_affinity_hit: true,
                materialization_window_limit: expected_window,
            }
        );
    }

    // 4/5 items queued on `w2` = 8000 bps (`Saturated`!). Next item with `CTX_WARM_A`
    // cleanly falls back to cold `w1` (`context_affinity_hit == false`).
    assert_eq!(w2.pressure_level(), DomainPressureLevel::Saturated);

    let mut fallback_item = make_item(9, RG_ALPHA, 100, DeadlineClass::Interactive, 0);
    fallback_item.steal_envelope.preferred_context_digest = Some(CTX_WARM_A);
    let fallback_adm = cell.place_runnable(fallback_item, 5);
    assert_eq!(
        fallback_adm,
        SchedulerAdmission::Admitted {
            assigned_domain: w1.domain_id(),
            effective_priority: 100,
            context_affinity_hit: false,
            materialization_window_limit: 16,
        }
    );

    // Verify warm-context preference during work stealing as well: if `w1` warms
    // `CTX_WARM_A`, `w1` steals a warm-matching item with `context_locality_hit == true`.
    w1.register_warm_context(CTX_WARM_A);
    let steal = w1.steal_from_peer_worker(&w2, 10).expect("warm steal");
    assert!(steal.stolen_item_id.is_some());
    assert!(steal.context_locality_hit);
}

#[test]
fn sched_backpressure_001_reverse_propagation_worker_to_cell_to_global_shrinks_window_and_returns_waiting_resource()
 {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 16);
    let cell = scheduler.add_cell(CELL_A_ID, 16);
    cell.register_resource_group(default_rg_policy(RG_ALPHA));

    let worker = cell.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 16,
        },
        default_capabilities(),
    );

    assert_eq!(scheduler.global().effective_materialization_window(), 16);
    assert_eq!(cell.effective_materialization_window(), 16);

    // Downstream `ModelSlot` saturates on `worker` -> propagates Worker -> Cell -> Global!
    worker.set_saturation_source(SaturationSource::ModelSlot, true);

    let cell_sig = cell.current_backpressure_signal();
    assert_eq!(cell_sig.origin_domain, worker.domain_id());
    assert_eq!(cell_sig.propagated_via, Some(cell.domain_id()));
    assert_eq!(cell_sig.level, DomainPressureLevel::Saturated);
    assert_eq!(cell_sig.primary_source, Some(SaturationSource::ModelSlot));
    assert_eq!(cell_sig.recommended_materialization_window_limit, 4);
    assert_eq!(
        cell_sig.admission_directive,
        AdmissionDirective::WaitingResource
    );

    let global_sig = scheduler.global().current_backpressure_signal();
    assert_eq!(global_sig.origin_domain, worker.domain_id());
    assert_eq!(
        global_sig.propagated_via,
        Some(scheduler.global().domain_id())
    );
    assert_eq!(global_sig.level, DomainPressureLevel::Saturated);
    assert_eq!(global_sig.primary_source, Some(SaturationSource::ModelSlot));
    assert_eq!(global_sig.recommended_materialization_window_limit, 4);
    assert_eq!(
        global_sig.admission_directive,
        AdmissionDirective::WaitingResource
    );

    // Non-critical admission returns `WaitingResource` instead of unbounded queueing.
    let std_item = make_item(40, RG_ALPHA, 100, DeadlineClass::Standard, 100);
    assert_eq!(
        cell.place_runnable(std_item, 100),
        SchedulerAdmission::WaitingResource {
            reporting_domain: cell.domain_id(),
            level: DomainPressureLevel::Saturated,
            source: Some(SaturationSource::ModelSlot),
            materialization_window_limit: 4,
        }
    );
    assert_eq!(worker.total_queued_len(), 0);

    // Escalate to `Critical` overload -> materialization window contracts to 0,
    // blocking even `Immediate` admissions.
    worker.set_critical_overload(true);
    assert_eq!(cell.effective_materialization_window(), 0);
    assert_eq!(scheduler.global().effective_materialization_window(), 0);

    let imm_item = make_item(41, RG_ALPHA, 200, DeadlineClass::Immediate, 110);
    assert!(cell.place_runnable(imm_item, 110).is_waiting_resource());

    // Clear overload & saturation -> pressure recovers `Worker -> Cell -> Global`.
    worker.set_critical_overload(false);
    worker.set_saturation_source(SaturationSource::ModelSlot, false);
    assert_eq!(cell.effective_materialization_window(), 16);
    assert_eq!(scheduler.global().effective_materialization_window(), 16);
    assert!(cell.place_runnable(std_item, 120).is_admitted());
}

#[test]
fn sched_backpressure_001_cross_cell_pressure_aware_placement_sheds_overloaded_cell_to_healthy_cell()
 {
    let scheduler = ThreeTierScheduler::new(GLOBAL_ID, 16);
    let cell_a = scheduler.add_cell(CELL_A_ID, 8);
    let cell_b = scheduler.add_cell(CELL_B_ID, 8);

    cell_a.register_resource_group(default_rg_policy(RG_ALPHA));
    cell_b.register_resource_group(default_rg_policy(RG_ALPHA));

    let worker_a = cell_a.add_worker(
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 8,
        },
        default_capabilities(),
    );
    let worker_b = cell_b.add_worker(
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 8,
            base_materialization_window: 8,
        },
        default_capabilities(),
    );

    // Initially `Cell A` is healthy, so preferred placement onto `Cell A` lands on `worker_a`.
    let first = make_item(50, RG_ALPHA, 100, DeadlineClass::Interactive, 0);
    let adm_initial = scheduler.place_across_cells(first, Some(CELL_A_ID), 0);
    assert_eq!(
        adm_initial,
        SchedulerAdmission::Admitted {
            assigned_domain: worker_a.domain_id(),
            effective_priority: 100,
            context_affinity_hit: false,
            materialization_window_limit: 8,
        }
    );

    // Saturate `DriverPool` on `worker_a` -> `Cell A` becomes `Saturated`, Global
    // shrinks its total window (16 -> 10) and sheds `preferred_cell_id = Some(CELL_A_ID)`
    // placement onto healthy `Cell B` (`worker_b`)!
    worker_a.set_saturation_source(SaturationSource::DriverPool, true);

    let cell_a_summary = scheduler
        .global()
        .inspect_cell_pressure(CELL_A_ID)
        .expect("cell A summary");
    assert_eq!(cell_a_summary.level, DomainPressureLevel::Saturated);
    assert_eq!(
        cell_a_summary.primary_source,
        Some(SaturationSource::DriverPool)
    );
    assert_eq!(scheduler.global().effective_materialization_window(), 10);

    let shed_item = make_item(51, RG_ALPHA, 100, DeadlineClass::Interactive, 10);
    let adm_shed = scheduler.place_across_cells(shed_item, Some(CELL_A_ID), 10);
    assert_eq!(
        adm_shed,
        SchedulerAdmission::Admitted {
            assigned_domain: worker_b.domain_id(),
            effective_priority: 100,
            context_affinity_hit: false,
            materialization_window_limit: 8,
        }
    );

    // Now saturate `StorageIo` on `worker_b` as well -> all Cells saturated,
    // so Global placement returns `WaitingResource` instead of queueing!
    worker_b.set_saturation_source(SaturationSource::StorageIo, true);
    let blocked_item = make_item(52, RG_ALPHA, 100, DeadlineClass::Standard, 20);
    let adm_blocked = scheduler.place_across_cells(blocked_item, Some(CELL_A_ID), 20);
    assert!(adm_blocked.is_waiting_resource());
}

#[test]
fn sched_hier_001_cross_cell_raw_worker_steal_is_forbidden_fail_closed() {
    let w_cell_a = WorkerSchedulerDomain::new(
        CELL_A_ID,
        WORKER_1_ID,
        WorkerDomainConfig {
            max_concurrency: 4,
            base_materialization_window: 8,
        },
        default_capabilities(),
    );
    let w_cell_b = WorkerSchedulerDomain::new(
        CELL_B_ID,
        WORKER_2_ID,
        WorkerDomainConfig {
            max_concurrency: 4,
            base_materialization_window: 8,
        },
        default_capabilities(),
    );

    w_cell_a.register_resource_group(default_rg_policy(RG_ALPHA));
    w_cell_b.register_resource_group(default_rg_policy(RG_ALPHA));
    assert!(
        w_cell_a
            .enqueue(make_item(60, RG_ALPHA, 100, DeadlineClass::Standard, 0), 0)
            .is_admitted()
    );

    assert_eq!(
        w_cell_b.steal_from_peer_worker(&w_cell_a, 10),
        Err(StealError::CrossCellStealForbidden)
    );
    assert_eq!(
        w_cell_a.steal_from_peer_worker(&w_cell_a, 10),
        Err(StealError::SelfSteal)
    );
    assert_eq!(w_cell_a.runnable_queue_len(), 1);
    assert_eq!(w_cell_b.runnable_queue_len(), 0);
}
