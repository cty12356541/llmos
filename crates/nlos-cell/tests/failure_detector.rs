//! Failure detector: escalation monotonicity, heartbeat clears suspicion,
//! dead incarnations never revive, and the epoch-loss closure contract.
//!
//! All tests build fence snapshots directly (`CellFence::present`), so no
//! test claims the process-scoped `CellAuthority`.

use std::num::NonZeroU64;

use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, DeadJudgment, FailureDetector,
    FailureDetectorConfig, FailureDetectorError, Liveness, MonitoredSubject,
};
use nlos_types::{ExecutionFiberId, Generation, ProcessId, SchedulerDomainId};

const SUSPECT_AFTER: u64 = 5;
const DEAD_AFTER: u64 = 10;

fn config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(SUSPECT_AFTER).expect("suspect window"),
        NonZeroU64::new(DEAD_AFTER).expect("dead window"),
    )
    .expect("valid config")
}

fn fence(epoch: CellEpoch) -> CellFence {
    CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xdd; 16])),
        Generation::INITIAL,
        epoch,
        CellFencingToken::INITIAL,
    )
}

fn fiber(byte: u8) -> MonitoredSubject {
    MonitoredSubject::Fiber(ExecutionFiberId::from_bytes([byte; 16]))
}

fn process(byte: u8) -> MonitoredSubject {
    MonitoredSubject::Process(ProcessId::from_bytes([byte; 16]))
}

#[test]
fn config_rejects_dead_threshold_before_suspect() {
    let error = FailureDetectorConfig::new(
        NonZeroU64::new(DEAD_AFTER).expect("dead window"),
        NonZeroU64::new(SUSPECT_AFTER).expect("suspect window"),
    )
    .expect_err("dead before suspect must fail");
    assert_eq!(
        error,
        FailureDetectorError::DeadThresholdBeforeSuspect {
            suspect_after: NonZeroU64::new(DEAD_AFTER).expect("dead window"),
            dead_after: NonZeroU64::new(SUSPECT_AFTER).expect("suspect window"),
        }
    );
    let equal = FailureDetectorConfig::new(
        NonZeroU64::new(7).expect("suspect window"),
        NonZeroU64::new(7).expect("dead window"),
    );
    assert!(equal.is_ok(), "dead_after == suspect_after is reachable");
}

#[test]
fn silence_escalates_alive_suspect_dead_monotonically() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let subject = fiber(0x01);
    assert!(detector.liveness(subject).is_none());

    detector
        .record_heartbeat(subject, 100)
        .expect("first heartbeat");
    let view = detector.liveness(subject).expect("tracked");
    assert_eq!(view.liveness, Liveness::Alive);
    assert_eq!(view.epoch, CellEpoch::INITIAL);
    assert_eq!(view.incarnation, 1);
    assert_eq!(view.last_heartbeat, Some(100));

    // 4 ticks of silence: still alive.
    let report = detector.sweep(104).expect("sweep");
    assert_eq!(report.newly_suspected.len(), 0);
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Alive
    );

    // Exactly suspect_after silence: suspect at first crossing, monotone
    // afterwards (suspected_at keeps its first tick).
    let report = detector.sweep(105).expect("sweep");
    assert_eq!(report.newly_suspected, vec![(subject, 105)]);
    let report = detector.sweep(109).expect("sweep");
    assert!(report.newly_suspected.is_empty() && report.newly_dead.is_empty());
    let view = detector.liveness(subject).expect("tracked");
    assert_eq!(view.liveness, Liveness::Suspect);
    assert_eq!(view.suspected_at, Some(105));

    // Exactly dead_after silence: dead judgment closes with incarnation.
    let report = detector.sweep(110).expect("sweep");
    assert_eq!(
        report.newly_dead,
        vec![DeadJudgment {
            subject,
            epoch: CellEpoch::INITIAL,
            dead_at: 110,
            incarnation: 1,
        }]
    );
    // Idempotent at fixed tick and stable beyond.
    let report = detector.sweep(110).expect("sweep again");
    assert!(report.newly_dead.is_empty() && report.newly_suspected.is_empty());
    let report = detector.sweep(1_000).expect("sweep later");
    assert!(report.newly_dead.is_empty(), "dead stays closed");
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Dead
    );
    assert_eq!(
        detector.dead_judgments(),
        &[DeadJudgment {
            subject,
            epoch: CellEpoch::INITIAL,
            dead_at: 110,
            incarnation: 1,
        }]
    );
}

#[test]
fn heartbeat_clears_suspicion_but_never_revives_a_dead_incarnation() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let subject = process(0x02);
    detector
        .record_heartbeat(subject, 0)
        .expect("first heartbeat");

    // Suspect, then a late heartbeat proves liveness.
    detector.sweep(SUSPECT_AFTER).expect("sweep to suspect");
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Suspect
    );
    let outcome = detector
        .record_heartbeat(subject, 6)
        .expect("late heartbeat");
    assert!(outcome.cleared_suspicion);
    let view = detector.liveness(subject).expect("tracked");
    assert_eq!(view.liveness, Liveness::Alive);
    assert_eq!(view.suspected_at, None);
    assert_eq!(view.last_heartbeat, Some(6));
    // A second heartbeat while alive clears nothing.
    let outcome = detector
        .record_heartbeat(subject, 7)
        .expect("second heartbeat");
    assert!(!outcome.cleared_suspicion);

    // Let the subject die, then a further heartbeat is a typed reject.
    let report = detector.sweep(7 + DEAD_AFTER).expect("sweep to dead");
    assert_eq!(report.newly_dead.len(), 1);
    let error = detector
        .record_heartbeat(subject, 7 + DEAD_AFTER + 1)
        .expect_err("dead incarnation cannot revive");
    assert_eq!(
        error,
        FailureDetectorError::SubjectAlreadyDead {
            subject,
            epoch: CellEpoch::INITIAL,
            incarnation: 1,
        }
    );
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Dead
    );
    assert_eq!(detector.dead_judgments().len(), 1);
}

#[test]
fn caller_clock_must_be_monotone() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let subject = fiber(0x03);
    detector.record_heartbeat(subject, 50).expect("heartbeat");
    assert_eq!(
        detector.sweep(49).expect_err("regressed sweep"),
        FailureDetectorError::ClockRegression {
            presented: 49,
            last_seen: 50,
        }
    );
    assert_eq!(
        detector
            .record_heartbeat(subject, 48)
            .expect_err("regressed heartbeat"),
        FailureDetectorError::ClockRegression {
            presented: 48,
            last_seen: 50,
        }
    );
    // Same tick is not a regression (idempotent clock).
    detector.sweep(50).expect("same-tick sweep");
    detector
        .record_heartbeat(subject, 50)
        .expect("same-tick heartbeat");
}

#[test]
fn epoch_advance_voids_suspicions_and_never_promotes_them_to_dead() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let subject = fiber(0x04);
    detector.record_heartbeat(subject, 20).expect("heartbeat");
    detector
        .sweep(20 + SUSPECT_AFTER)
        .expect("sweep to suspect");
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Suspect
    );

    // Epoch advances while the subject is only suspected.
    let epoch2 = CellEpoch::INITIAL.checked_next().expect("epoch 2");
    let report = detector
        .on_epoch_advanced(&fence(epoch2))
        .expect("advance epoch");
    assert_eq!(report.previous_epoch, CellEpoch::INITIAL);
    assert_eq!(report.new_epoch, epoch2);
    assert_eq!(
        report.suspicions_voided,
        vec![(subject, 20 + SUSPECT_AFTER)]
    );
    assert_eq!(report.re_tracked_after_death.len(), 0);
    assert_eq!(detector.epoch(), epoch2);

    let view = detector.liveness(subject).expect("tracked");
    assert_eq!(view.liveness, Liveness::Alive);
    assert_eq!(view.epoch, epoch2);
    assert_eq!(view.last_heartbeat, None);

    // Silence is re-measured from the epoch-advance tick: had the old
    // heartbeat at tick 20 counted, this sweep (9 ticks past it) would keep
    // the subject suspected.
    let report = detector.sweep(29).expect("sweep in new epoch");
    assert_eq!(report.epoch, epoch2);
    assert!(report.newly_suspected.is_empty() && report.newly_dead.is_empty());
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Alive
    );

    // Escalation restarts from scratch: suspect at boundary + suspect_after.
    let report = detector
        .sweep(20 + SUSPECT_AFTER + SUSPECT_AFTER)
        .expect("sweep");
    assert_eq!(
        report.newly_suspected,
        vec![(subject, 20 + SUSPECT_AFTER + SUSPECT_AFTER)]
    );
    // Boundary + 9 is past the pre-fence heartbeat's dead window (dead
    // would close at 30 from tick 20), but no dead judgment may form from
    // old-epoch evidence.
    let report = detector.sweep(20 + SUSPECT_AFTER + 9).expect("sweep");
    assert_eq!(report.newly_dead.len(), 0);
    assert_eq!(detector.dead_judgments().len(), 0);
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Suspect
    );

    // Dead closes only from boundary-measured silence (boundary 25 + 10).
    let report = detector
        .sweep(20 + SUSPECT_AFTER + DEAD_AFTER)
        .expect("sweep");
    assert_eq!(
        report.newly_dead,
        vec![DeadJudgment {
            subject,
            epoch: epoch2,
            dead_at: 20 + SUSPECT_AFTER + DEAD_AFTER,
            incarnation: 1,
        }]
    );

    // Fresh heartbeat in the new epoch keeps a live subject alive.
    let mut detector = FailureDetector::new(&fence(epoch2), config());
    detector
        .record_heartbeat(subject, 40)
        .expect("fresh epoch heartbeat");
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Alive
    );
}

#[test]
fn epoch_advance_retracks_dead_subjects_as_new_incarnations() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let subject = process(0x05);
    detector.record_heartbeat(subject, 0).expect("heartbeat");
    detector.sweep(DEAD_AFTER).expect("sweep to dead");
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Dead
    );

    let epoch2 = CellEpoch::INITIAL.checked_next().expect("epoch 2");
    let report = detector
        .on_epoch_advanced(&fence(epoch2))
        .expect("advance epoch");
    assert_eq!(report.suspicions_voided.len(), 0);
    assert_eq!(report.re_tracked_after_death, vec![subject]);

    // The old judgment stays immutable history; the view opens a fresh
    // incarnation 2 with no evidence yet.
    assert_eq!(detector.dead_judgments().len(), 1);
    let view = detector.liveness(subject).expect("tracked");
    assert_eq!(view.liveness, Liveness::Alive);
    assert_eq!(view.incarnation, 2);
    assert_eq!(view.epoch, epoch2);
    assert_eq!(view.last_heartbeat, None);

    // A heartbeat is accepted again in the new incarnation; the subject
    // can die again and history records a second distinct judgment.
    detector
        .record_heartbeat(subject, 10)
        .expect("heartbeat in incarnation 2");
    detector.sweep(10 + DEAD_AFTER).expect("second death");
    assert_eq!(detector.dead_judgments().len(), 2);
    let second = detector.dead_judgments()[1];
    assert_eq!(second.incarnation, 2);
    assert_eq!(second.epoch, epoch2);
    assert_eq!(
        detector.liveness(subject).expect("tracked").liveness,
        Liveness::Dead
    );
}

#[test]
fn epoch_advance_rejects_identity_mismatch_and_non_advancing_epochs() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let epoch2 = CellEpoch::INITIAL.checked_next().expect("epoch 2");

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xee; 16])),
        Generation::INITIAL,
        epoch2,
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        detector
            .on_epoch_advanced(&foreign)
            .expect_err("foreign identity"),
        FailureDetectorError::IdentityMismatch {
            presented: foreign.identity(),
            current: detector.identity(),
        }
    );

    // Equal epoch is a typed reject; a real advance is accepted; then both
    // an equal repeat and a genuinely older epoch are typed rejects.
    assert_eq!(
        detector
            .on_epoch_advanced(&fence(CellEpoch::INITIAL))
            .expect_err("equal epoch"),
        FailureDetectorError::EpochNotAdvanced {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL,
        }
    );
    detector
        .on_epoch_advanced(&fence(epoch2))
        .expect("valid advance");
    assert_eq!(
        detector
            .on_epoch_advanced(&fence(epoch2))
            .expect_err("repeat advance"),
        FailureDetectorError::EpochNotAdvanced {
            presented: epoch2,
            current: epoch2,
        }
    );
    assert_eq!(
        detector
            .on_epoch_advanced(&fence(CellEpoch::INITIAL))
            .expect_err("older epoch after advance"),
        FailureDetectorError::EpochNotAdvanced {
            presented: CellEpoch::INITIAL,
            current: epoch2,
        }
    );
}

#[test]
fn fiber_and_process_ids_stay_type_distinct_and_reports_sort_by_subject() {
    let mut detector = FailureDetector::new(&fence(CellEpoch::INITIAL), config());
    let fiber_a = fiber(0x06);
    let fiber_b = fiber(0x07);
    let process_a = process(0x06);
    detector.record_heartbeat(fiber_a, 0).expect("heartbeat");
    detector.record_heartbeat(fiber_b, 0).expect("heartbeat");
    detector.record_heartbeat(process_a, 0).expect("heartbeat");
    assert_eq!(detector.subject_count(), 3);
    assert_ne!(fiber_a, process_a);

    // All three silent past dead_after: judgments close for each, sorted
    // deterministically by subject.
    let report = detector.sweep(DEAD_AFTER).expect("sweep");
    assert_eq!(report.newly_dead.len(), 3);
    let mut subjects: Vec<_> = report
        .newly_dead
        .iter()
        .map(|judgment| judgment.subject)
        .collect();
    let mut sorted = subjects.clone();
    sorted.sort();
    assert_eq!(subjects, sorted);
    subjects.sort_by_key(|subject| match subject {
        MonitoredSubject::Fiber(_) => 0,
        MonitoredSubject::Process(_) => 1,
    });
    assert_eq!(subjects.len(), 3, "fiber/process variants both tracked");
}
