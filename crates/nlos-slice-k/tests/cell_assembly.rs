//! W48: the [`CellHost`] seven-piece Cell assembly.
//!
//! The closed loop under test (per the cell-assembly wiring map): one host
//! wraps one [`SliceKRuntime`] plus the Cell-local pieces, an epoch advance
//! driven by the single authority holder fans out to every snapshot
//! consumer in one call (leases quarantined, detector suspicions voided,
//! cache entries fenced), the registration-as-heartbeat wiring feeds the
//! detector real durable-clock evidence, and terminal observations record
//! no heartbeat.
//!
//! One `#[tokio::test]` so this file claims the process-scoped Cell exactly
//! once (the `nlos-cell` claim slot is per process and never released);
//! tokio is needed only to hand the outbox-pump piece a real adapter.

use std::num::NonZeroU64;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_cell::{
    CellEpoch, CellIdentity, FailureDetectorConfig, InsertOutcome, Liveness, MonitoredSubject,
    NamePath,
};
use nlos_driver_mock::CacheHealth;
use nlos_lease::{QuotaLeaseGrantError, QuotaLeaseState};
use nlos_process::ProcessBindingRecord;
use nlos_runtime_tokio::{PumpState, TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{CellEpochAdvance, CellHost, CellHostConfig, SliceKError};
use nlos_types::{
    CapabilityId, Generation, QuotaLeaseId, SchedulerDomainId, TaskAttemptId, TaskId,
};

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-cell-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

/// Escalation thresholds in durable-clock ticks: suspect after 3 silent
/// ticks, dead after 5.
fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("threshold ordering")
}

/// Sweeps until `subject` is `Suspect` (bounded: one tick per sweep, the
/// silence window is three).
fn sweep_until_suspect(host: &mut CellHost, subject: MonitoredSubject) {
    for _ in 0..6 {
        let report = host.sweep_liveness().expect("sweep");
        if report.newly_suspected.iter().any(|(s, _)| *s == subject) {
            assert_eq!(
                host.failure_detector()
                    .liveness(subject)
                    .expect("view")
                    .liveness,
                Liveness::Suspect,
                "subject must read Suspect once a sweep reported it"
            );
            return;
        }
    }
    panic!("subject never escalated to Suspect within the sweep bound");
}

/// The seven pieces present and inspectable straight after assembly, plus
/// the outbox-pump piece actually running (and stopping) under the host.
fn assert_seven_pieces_assembled(host: &mut CellHost, domain: SchedulerDomainId) {
    let inspect = host.inspect_assembly().expect("assembly inspect");
    assert_eq!(inspect.cell_identity, CellIdentity::from_domain(domain));
    assert_eq!(inspect.node_boot_generation, Generation::INITIAL);
    assert_eq!(inspect.epoch, CellEpoch::INITIAL);
    assert_eq!(inspect.fencing_token.get(), 1);
    assert_eq!(inspect.quota_available, 1000);
    // The runtime's open already registered the semantic-writer delegated
    // process, so the supervisor face lists exactly that one binding.
    assert_eq!(inspect.active_process_bindings, 1);
    assert_eq!(inspect.gateway_health, CacheHealth::Healthy);
    assert_eq!(inspect.outbox_pump, None);
    assert!(inspect.artifact_store_responds);
    assert_eq!(inspect.detector_epoch, CellEpoch::INITIAL);
    assert_eq!(inspect.detector_subjects, 0);
    assert_eq!(inspect.detector_dead_judgments, 0);
    assert_eq!(inspect.name_cache_epoch, CellEpoch::INITIAL);
    assert_eq!(inspect.name_cache_paths, 0);
    assert!(
        inspect
            .report_lines()
            .iter()
            .any(|line| line.starts_with("cell=")),
        "report lines must lead with the cell identity: {:?}",
        inspect.report_lines()
    );

    let adapter = TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter");
    host.runtime().start_pump(&adapter).expect("start pump");
    let pump_state = host
        .inspect_assembly()
        .expect("pump inspect")
        .outbox_pump
        .expect("running pump health");
    assert_eq!(
        pump_state.state,
        PumpState::Running,
        "the outbox piece must come up Running on the host's runtime"
    );
    host.runtime().stop_pump();
    assert_eq!(
        host.inspect_assembly().expect("post-stop").outbox_pump,
        None
    );
}

/// The heartbeat wiring: registration observed = liveness evidence, replay
/// is idempotent but still fresh evidence, silence escalates, and a direct
/// heartbeat clears the unproven suspicion. Returns the tracked subject.
fn assert_heartbeat_lane(host: &mut CellHost) -> (MonitoredSubject, ProcessBindingRecord) {
    let task_id = TaskId::from_bytes([0x31; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x32; 16]);
    let binding = host
        .spawn_monitored_process(0xB1, task_id, attempt_id, Generation::INITIAL)
        .expect("spawn monitored process");
    let subject = MonitoredSubject::Process(binding.process_id);
    let first_view = host.failure_detector().liveness(subject).expect("tracked");
    assert_eq!(first_view.liveness, Liveness::Alive);
    let first_beat = first_view.last_heartbeat.expect("beat at registration");
    assert_eq!(
        host.inspect_assembly()
            .expect("binding inspect")
            .active_process_bindings,
        2,
        "writer binding plus the freshly spawned monitored process"
    );

    // Replay: same registration replays idempotently and is still fresh
    // evidence (one subject, a strictly newer beat tick).
    let replayed = host
        .spawn_monitored_process(0xB1, task_id, attempt_id, Generation::INITIAL)
        .expect("replay monitored process");
    assert_eq!(replayed.process_id, binding.process_id);
    assert_eq!(host.failure_detector().subject_count(), 1);
    let second_beat = host
        .failure_detector()
        .liveness(subject)
        .expect("tracked")
        .last_heartbeat
        .expect("beat at replay");
    assert!(
        second_beat > first_beat,
        "replay must take a fresh durable tick"
    );

    // Silence escalates: sweep once at zero elapsed (nothing), then until
    // the three-tick window suspects the subject.
    let quiet = host.sweep_liveness().expect("sweep at zero elapsed");
    assert_eq!(quiet.newly_suspected, Vec::<(MonitoredSubject, u64)>::new());
    sweep_until_suspect(host, subject);

    // A direct heartbeat clears the unproven suspicion.
    let outcome = host
        .record_process_heartbeat(binding.process_id)
        .expect("clearing heartbeat");
    assert!(outcome.cleared_suspicion);
    assert_eq!(
        host.failure_detector()
            .liveness(subject)
            .expect("view")
            .liveness,
        Liveness::Alive
    );
    (subject, binding)
}

/// The name-cache piece (insert serves a hit; identical replay is a typed
/// no-op) and the lease piece (grant deducts, identical re-grant replays
/// without a second deduct).
fn assert_cache_and_lease_pieces(host: &mut CellHost) -> (NamePath, QuotaLeaseId) {
    let cap_path = NamePath::new("/w48/capability").expect("path");
    let capability = CapabilityId::from_bytes([0x51; 16]);
    assert_eq!(
        host.insert_cached_name(cap_path.clone(), capability, Generation::INITIAL)
            .expect("insert"),
        InsertOutcome::Inserted
    );
    let hit = host.cached_name(&cap_path).expect("cache hit");
    assert_eq!(hit.cached.capability, capability);
    assert_eq!(hit.inserted_epoch, CellEpoch::INITIAL);
    assert_eq!(
        host.insert_cached_name(cap_path.clone(), capability, Generation::INITIAL)
            .expect("replay insert"),
        InsertOutcome::Unchanged
    );

    let lease_id = QuotaLeaseId::from_bytes([0x61; 16]);
    let grant = host
        .grant_quota_lease(lease_id, 40, None)
        .expect("grant quota lease");
    assert_eq!(grant.state(), QuotaLeaseState::Issued);
    assert_eq!(grant.epoch(), CellEpoch::INITIAL);
    assert_eq!(host.quota_available(), 960);

    // Re-granting the identical lease under the same fence is the
    // idempotent no-deduct replay.
    let fence_at_grant = host.fence();
    let replayed_grant = host
        .grant_quota_lease_against(&fence_at_grant, lease_id, 40, None)
        .expect("grant replay");
    assert_eq!(replayed_grant.state(), QuotaLeaseState::Issued);
    assert_eq!(host.quota_available(), 960);
    (cap_path, lease_id)
}

/// The three-way epoch linkage — one advance fences all three snapshot
/// consumers — plus the post-advance idempotent and stale-fence faces.
fn assert_epoch_three_way_linkage(
    host: &mut CellHost,
    subject: MonitoredSubject,
    cap_path: &NamePath,
    lease_id: QuotaLeaseId,
) -> CellEpochAdvance {
    sweep_until_suspect(host, subject);
    let fence_before = host.fence();

    let advance = host.advance_cell_epoch().expect("epoch advance");
    let epoch_two = CellEpoch::INITIAL.checked_next().expect("second epoch");
    assert_eq!(advance.fence.epoch(), epoch_two);
    assert_eq!(advance.fence.fencing_token().get(), 2);
    assert_eq!(host.fence(), advance.fence);

    // (1) Lease loss: the unreconciled lease is quarantined, not refunded.
    let quarantined = host.query_quota_lease(lease_id).expect("query lease");
    assert_eq!(quarantined.state(), QuotaLeaseState::Quarantined);
    assert_eq!(host.quota_available(), 960, "quarantine never refunds");

    // (2) Detector: the suspicion voided at the boundary, heartbeat
    // evidence does not cross, the subject stays tracked in the new epoch.
    assert_eq!(advance.detector.previous_epoch, CellEpoch::INITIAL);
    assert_eq!(advance.detector.new_epoch, epoch_two);
    assert_eq!(advance.detector.suspicions_voided.len(), 1);
    assert_eq!(advance.detector.suspicions_voided[0].0, subject);
    assert_eq!(
        advance.detector.re_tracked_after_death,
        Vec::<MonitoredSubject>::new()
    );
    let view = host.failure_detector().liveness(subject).expect("view");
    assert_eq!(view.liveness, Liveness::Alive);
    assert_eq!(
        view.last_heartbeat, None,
        "old-epoch evidence never crosses"
    );
    assert_eq!(view.suspected_at, None, "unproven suspicion voided");
    assert_eq!(host.failure_detector().epoch(), epoch_two);

    // (3) Name cache: the live entry fenced at the boundary (a miss is
    // `None`, never an existence leak), and every epoch axis moved.
    assert_eq!(advance.name_cache.previous_epoch, CellEpoch::INITIAL);
    assert_eq!(advance.name_cache.new_epoch, epoch_two);
    assert_eq!(advance.name_cache.entries_fenced, 1);
    assert_eq!(host.cached_name(cap_path), None);
    let post = host.inspect_assembly().expect("post-advance inspect");
    assert_eq!(post.epoch, epoch_two);
    assert_eq!(post.detector_epoch, epoch_two);
    assert_eq!(post.name_cache_epoch, epoch_two);

    // Same lease id + face value under the new fence: the committed lease
    // replays (still quarantined, still no refund).
    let quarantined_replay = host
        .grant_quota_lease_against(&advance.fence, lease_id, 40, None)
        .expect("quarantined grant replay");
    assert_eq!(quarantined_replay.state(), QuotaLeaseState::Quarantined);
    assert_eq!(host.quota_available(), 960);

    // A new lease under the pre-advance fence: typed stale-epoch reject.
    let stale = host.grant_quota_lease_against(
        &fence_before,
        QuotaLeaseId::from_bytes([0x62; 16]),
        5,
        None,
    );
    assert!(matches!(
        stale,
        Err(SliceKError::Lease(QuotaLeaseGrantError::StaleEpoch { .. }))
    ));

    // The cache still serves its new epoch: a fresh insert above the old
    // entry's generation is accepted at the new epoch.
    let generation_three = Generation::new(NonZeroU64::new(3).expect("generation three"));
    assert_eq!(
        host.insert_cached_name(
            cap_path.clone(),
            CapabilityId::from_bytes([0x51; 16]),
            generation_three
        )
        .expect("post-advance insert"),
        InsertOutcome::Inserted
    );
    assert_eq!(
        host.cached_name(cap_path)
            .expect("new-epoch hit")
            .inserted_epoch,
        epoch_two
    );
    advance
}

/// Terminal observation records no heartbeat (silence, not evidence), and
/// replays just as silently.
fn assert_terminal_records_no_heartbeat(
    host: &mut CellHost,
    binding: &ProcessBindingRecord,
    subject: MonitoredSubject,
) {
    host.record_process_heartbeat(binding.process_id)
        .expect("pre-terminal heartbeat");
    let beat_before_terminal = host
        .failure_detector()
        .liveness(subject)
        .expect("view")
        .last_heartbeat
        .expect("a recorded beat to compare against");
    let terminal = host
        .terminate_monitored_process(binding, 0xB2)
        .expect("terminate");
    assert_eq!(terminal.process_id, binding.process_id);
    assert_eq!(
        host.failure_detector()
            .liveness(subject)
            .expect("view")
            .last_heartbeat,
        Some(beat_before_terminal),
        "termination must not record liveness evidence"
    );
    let terminal_replay = host
        .terminate_monitored_process(binding, 0xB2)
        .expect("terminate replay");
    assert_eq!(terminal_replay.idempotency_key, terminal.idempotency_key);
    assert_eq!(
        host.failure_detector()
            .liveness(subject)
            .expect("view")
            .last_heartbeat,
        Some(beat_before_terminal)
    );
}

#[tokio::test]
async fn cell_host_assembles_seven_pieces_and_advances_epoch_with_broadcast() {
    let temp = TempDir::new("w48");
    let domain = SchedulerDomainId::from_bytes([0x48; 16]);
    let mut host = CellHost::open(
        temp.root(),
        CellHostConfig::new(domain, 1000, detector_config()),
    )
    .expect("assemble cell host");

    assert_seven_pieces_assembled(&mut host, domain);
    let (subject, binding) = assert_heartbeat_lane(&mut host);
    let (cap_path, lease_id) = assert_cache_and_lease_pieces(&mut host);
    assert_epoch_three_way_linkage(&mut host, subject, &cap_path, lease_id);
    assert_terminal_records_no_heartbeat(&mut host, &binding, subject);

    // The process-scoped claim: a second host in this process fails typed,
    // whatever root it points at.
    let second_temp = TempDir::new("w48-second");
    let second = CellHost::open(
        second_temp.root(),
        CellHostConfig::new(
            SchedulerDomainId::from_bytes([0x49; 16]),
            1,
            detector_config(),
        ),
    );
    assert!(matches!(
        second,
        Err(SliceKError::Cell(
            nlos_cell::CellError::AlreadyClaimedInProcess { .. }
        ))
    ));
}
