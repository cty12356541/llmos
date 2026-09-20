//! W31-C reclaim real-execution closure acceptance: a planned
//! [`WorkingSetReclaimExecution`] surfaced on an issued permit decision is
//! driven to completion through
//! [`SqliteTaskAuthority::drive_working_set_reclaim`], whose
//! `CheckpointEvict` phase closes evictable working-set members through the
//! public `close_permit` path. The mixed-state working set below pins the
//! honest selectivity: plain permits and `NoEffect`-slot permits are
//! evictable, `Planned`-slot and already-closed members are not, and the
//! readback (pressure snapshot, permit states, closure receipts) proves
//! execution rather than planning.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    Authorities, ClosePermitDecision, ClosePermitRequest, LogicalEffectDescriptor, NoEffectReason,
    NoEffectRequest, PermitClosureOutcome, PermitDecision, PermitRequest, PermitState,
    PlannedEffect, ReceiptOutcome, ReclaimPhase, ScaleProfile, SnapshotBundle, SqliteTaskAuthority,
    TaskSpec, TaskStoreError, WorkingSetReclaimAdvisory, WorkingSetReclaimExecution,
    WorkingSetReclaimExecutionRequest, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};

static RECLAIM_TEST_PROFILE: ScaleProfile = ScaleProfile {
    profile_id: "task-reclaim-drive-test",
    max_task_nodes: 64,
    max_task_registrations: 64,
    max_active_working_set: 8,
    reclaim_threshold_ratio: Some(50),
};

static SHORTFALL_TEST_PROFILE: ScaleProfile = ScaleProfile {
    profile_id: "task-reclaim-shortfall-test",
    max_task_nodes: 64,
    max_task_registrations: 64,
    max_active_working_set: 4,
    reclaim_threshold_ratio: Some(50),
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-reclaim-execution-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open_with_profile(&self, profile: &'static ScaleProfile) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open_with_scale_profile(&self.path, profile)
            .expect("open task authority with profile")
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
    authority
        .register_task(TaskSpec {
            task_id: task_id(index),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .expect("register task");
}

fn attempt_spec(index: u64) -> nlos_task::AttemptSpec {
    nlos_task::AttemptSpec {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(id_bytes(0x10, index)),
            snapshot_digest: [0x20; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(id_bytes(0xc0, index)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xa0, index)),
        registered_at_ms: 2_000,
    }
}

fn one_planned_effect(task_index: u64) -> Vec<PlannedEffect> {
    vec![PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(task_index),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x44; 32],
            stable_action_slot: 1,
            target_authority_object_id: [0x55; 32],
            effect_class: 7,
            idempotency_scope: 3,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }]
}

fn permit_request(index: u64, requested_at_ms: i64, planned: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: Generation::INITIAL,
        write_set_root: [0x33; 32],
        planned_effects: planned,
        idempotency_key: IdempotencyKey::from_bytes(id_bytes(0xb0, index)),
        valid_until_ms: 99_999,
        requested_at_ms,
    }
}

fn issue(
    authority: &SqliteTaskAuthority,
    index: u64,
    requested_at_ms: i64,
    planned: Vec<PlannedEffect>,
) -> nlos_task::CommitPermitDecision {
    register_task(authority, index);
    authority
        .register_attempt(attempt_spec(index))
        .expect("register attempt");
    authority
        .request_commit_permit_decision_with_authorities_struct(
            Authorities::default(),
            permit_request(index, requested_at_ms, planned),
        )
        .expect("request permit")
}

fn issued_permit(decision: &nlos_task::CommitPermitDecision) -> nlos_task::PermitRecord {
    match &decision.permit {
        PermitDecision::Issued(record) => (**record).clone(),
        other => panic!("expected Issued permit, got {other:?}"),
    }
}

fn close_public(
    authority: &SqliteTaskAuthority,
    index: u64,
    permit: &nlos_task::PermitRecord,
    outcome: PermitClosureOutcome,
    at_ms: i64,
) {
    match authority.close_permit(ClosePermitRequest {
        task_id: task_id(index),
        attempt_id: attempt_id(index),
        attempt_generation: permit.attempt_generation,
        permit_id: permit.permit_id,
        outcome,
        fenced_participant_digest: [0; 32],
        closed_at_ms: at_ms,
    }) {
        Ok(ClosePermitDecision::Closed(_)) => {}
        other => panic!("expected Closed, got {other:?}"),
    }
}

#[allow(clippy::too_many_lines)] // One test covers the full mixed-state closure and readback.
/// Given/When/Then: given a mixed-state working set (one already-closed
/// permit, four plain permits, one `Planned`-slot permit, one
/// `NoEffect`-slot permit) pressed past the soft threshold; when the
/// advisory-surfaced reclaim execution is driven to completion; then the
/// walk reports the three face-absent phases honestly, `CheckpointEvict`
/// closes exactly the overshoot through the public close path in FIFO
/// order, structurally non-evictable members stay `Issued`, and the
/// readback (pressure snapshot, permit states, closure receipts) proves
/// real execution rather than planning.
#[test]
fn drive_closes_mixed_working_set_through_public_closures() {
    let database = TestDatabase::new("mixed-closure");
    let authority = database.open_with_profile(&RECLAIM_TEST_PROFILE);
    assert_eq!(RECLAIM_TEST_PROFILE.reclaim_threshold_count(), 4);

    // Mixed state 1: a permit that already reached a terminal close —
    // durable, but not part of the active working set.
    let closed = issue(&authority, 90, 3_100, Vec::new());
    let closed_permit = issued_permit(&closed);
    close_public(
        &authority,
        90,
        &closed_permit,
        PermitClosureOutcome::FailedBeforeEffect,
        3_200,
    );

    // Mixed state 2: plain permits with no effect slots, one per task, in
    // rising FIFO order. The fourth issuance lands exactly at the soft
    // threshold (projected 4), so no advisory fires yet.
    let mut plain_permits = Vec::new();
    for (ordinal, index) in [1_u64, 2, 3, 4].into_iter().enumerate() {
        let decision = issue(
            &authority,
            index,
            3_300 + 100 * i64::try_from(ordinal).expect("ordinal fits"),
            Vec::new(),
        );
        let permit = issued_permit(&decision);
        assert!(
            decision.reclaim_advisory.is_none(),
            "projected {} must stay at or below the soft threshold",
            ordinal + 1
        );
        plain_permits.push(permit);
    }

    // Mixed state 3: a permit holding a still-`Planned` effect slot —
    // structurally not closable, therefore not reclaim-evictable. This
    // issuance crosses the soft threshold and surfaces the warrant.
    let planned_decision = issue(&authority, 5, 3_700, one_planned_effect(5));
    let planned_permit = issued_permit(&planned_decision);
    let warrant = planned_decision
        .reclaim_execution
        .expect("crossing issuance must surface the reclaim execution warrant");
    assert_eq!(warrant.advisory.projected_active_count, 5);

    // Mixed state 4: a permit whose only slot holds an authoritative
    // `NoEffect` absence proof — closable, therefore reclaim-evictable.
    let no_effect_decision = issue(&authority, 6, 3_800, one_planned_effect(6));
    let no_effect_permit = issued_permit(&no_effect_decision);
    authority
        .record_no_effect(NoEffectRequest {
            task_id: task_id(6),
            attempt_id: attempt_id(6),
            attempt_generation: no_effect_permit.attempt_generation,
            permit_id: no_effect_permit.permit_id,
            permit_epoch: no_effect_permit.permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: None,
            recorded_at_ms: 3_900,
        })
        .expect("record no effect");

    let before = authority
        .inspect_working_set_pressure()
        .expect("pressure before drive");
    assert_eq!(before.active_count, 6);
    assert!(before.needs_reclaim);

    let report = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: warrant,
            executed_at_ms: 9_000,
        })
        .expect("drive reclaim execution");

    // The walk covers the full default policy from the planned step, with
    // only `CheckpointEvict` carrying a real face in this authority.
    assert_eq!(
        report.phases,
        vec![
            nlos_task::WorkingSetReclaimPhaseReport {
                phase: ReclaimPhase::RebuildableCache,
                evicted_units: 0,
                face_absent: true,
            },
            nlos_task::WorkingSetReclaimPhaseReport {
                phase: ReclaimPhase::DegradeBackgroundQos,
                evicted_units: 0,
                face_absent: true,
            },
            nlos_task::WorkingSetReclaimPhaseReport {
                phase: ReclaimPhase::CheckpointEvict,
                evicted_units: 2,
                face_absent: false,
            },
            nlos_task::WorkingSetReclaimPhaseReport {
                phase: ReclaimPhase::Kill,
                evicted_units: 0,
                face_absent: true,
            },
        ]
    );
    assert_eq!(report.pre_active_count, 6);
    assert_eq!(report.post_active_count, 4);
    assert_eq!(report.reclaim_threshold_count, 4);
    assert!(report.pressure_relieved);

    // FIFO victims: the two oldest evictable permits are the first two
    // plain permits; the already-closed member never appears; the
    // `Planned`-slot member is skipped; the `NoEffect` member was not
    // needed for relief.
    assert_eq!(
        report
            .evictions
            .iter()
            .map(|eviction| eviction.permit_id)
            .collect::<Vec<_>>(),
        vec![plain_permits[0].permit_id, plain_permits[1].permit_id]
    );

    // Readback proves execution: pressure relieved, victims durably
    // `Closed` with `CancelledBeforeEffect` closure receipts, skipped
    // members still `Issued`.
    let after = authority
        .inspect_working_set_pressure()
        .expect("pressure after drive");
    assert_eq!(after.active_count, 4);
    assert!(!after.needs_reclaim);
    assert!(after.admits);

    for permit in [&plain_permits[0], &plain_permits[1]] {
        let inspected = authority
            .inspect_permit(permit.task_id, permit.permit_id)
            .expect("inspect evicted permit");
        assert_eq!(inspected.state, PermitState::Closed);
    }
    let inspected_planned = authority
        .inspect_permit(task_id(5), planned_permit.permit_id)
        .expect("inspect planned-slot permit");
    assert_eq!(inspected_planned.state, PermitState::Issued);
    let inspected_no_effect = authority
        .inspect_permit(task_id(6), no_effect_permit.permit_id)
        .expect("inspect no-effect permit");
    assert_eq!(inspected_no_effect.state, PermitState::Issued);

    for eviction in &report.evictions {
        let receipt = authority
            .inspect_receipt(eviction.task_id, eviction.closure_receipt_id)
            .expect("inspect closure receipt");
        assert_eq!(receipt.outcome, ReceiptOutcome::CancelledBeforeEffect);
        assert_eq!(receipt.permit_id, Some(eviction.permit_id));
    }
}

/// Given/When/Then: given a relieved working set (a prior drive already
/// brought the count back to the soft threshold); when the same warrant is
/// driven again; then the drive is the legal no-op — zero evictions, the
/// full phase walk, and the pressure readback unchanged.
#[test]
fn drive_after_relief_is_the_legal_noop() {
    let database = TestDatabase::new("noop-redrive");
    let authority = database.open_with_profile(&SHORTFALL_TEST_PROFILE);
    assert_eq!(SHORTFALL_TEST_PROFILE.reclaim_threshold_count(), 2);

    let mut permits = Vec::new();
    for index in 1..=3u64 {
        let decision = issue(
            &authority,
            index,
            3_000 + 100 * i64::try_from(index).expect("index fits"),
            Vec::new(),
        );
        permits.push(issued_permit(&decision));
    }
    let warrant = authority
        .inspect_working_set_pressure()
        .expect("pressure snapshot")
        .reclaim_advisory
        .map(|advisory| nlos_task::plan_working_set_reclaim_execution(&advisory))
        .expect("current advisory above threshold");

    let first = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: warrant,
            executed_at_ms: 9_000,
        })
        .expect("first drive");
    assert_eq!(first.pre_active_count, 3);
    assert_eq!(first.evictions.len(), 1);
    assert!(first.pressure_relieved);

    let second = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: warrant,
            executed_at_ms: 9_100,
        })
        .expect("second drive");
    assert!(second.evictions.is_empty());
    assert_eq!(second.post_active_count, 2);
    assert!(second.pressure_relieved);
    assert_eq!(second.phases.len(), 4);
}

/// Given/When/Then: given a working set pressed past the soft threshold
/// where every member holds a still-`Planned` effect slot; when the
/// reclaim execution is driven; then no member is evictable, the
/// `CheckpointEvict` phase reports a real zero with `face_absent == false`,
/// the pressure stays unrelieved (`pressure_relieved == false`, never
/// fabricated), and every permit stays `Issued`.
#[test]
fn drive_reports_honest_shortfall_when_nothing_is_evictable() {
    let database = TestDatabase::new("honest-shortfall");
    let authority = database.open_with_profile(&SHORTFALL_TEST_PROFILE);

    let mut permits = Vec::new();
    for index in 1..=3u64 {
        let decision = issue(
            &authority,
            index,
            3_000 + 100 * i64::try_from(index).expect("index fits"),
            one_planned_effect(index),
        );
        permits.push(issued_permit(&decision));
    }
    let warrant = authority
        .inspect_working_set_pressure()
        .expect("pressure snapshot")
        .reclaim_advisory
        .map(|advisory| nlos_task::plan_working_set_reclaim_execution(&advisory))
        .expect("current advisory above threshold");

    let report = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: warrant,
            executed_at_ms: 9_000,
        })
        .expect("drive with nothing evictable");
    assert_eq!(report.pre_active_count, 3);
    assert_eq!(report.post_active_count, 3);
    assert!(report.evictions.is_empty());
    assert!(!report.pressure_relieved);
    let checkpoint_evict = report
        .phases
        .iter()
        .find(|phase| phase.phase == ReclaimPhase::CheckpointEvict)
        .expect("checkpoint-evict phase");
    assert_eq!(checkpoint_evict.evicted_units, 0);
    assert!(!checkpoint_evict.face_absent);
    for permit in &permits {
        let inspected = authority
            .inspect_permit(permit.task_id, permit.permit_id)
            .expect("inspect untouched permit");
        assert_eq!(inspected.state, PermitState::Issued);
    }
}

/// Given/When/Then: given a warrant minted against a different tier, and a
/// warrant naming a phase sequence outside the default policy; when either
/// is driven; then the authority fails closed with the matching typed error
/// and the working-set readback is unchanged.
#[test]
fn drive_rejects_foreign_tier_warrant_and_out_of_range_sequence() {
    let database = TestDatabase::new("warrant-gates");
    let authority = database.open_with_profile(&RECLAIM_TEST_PROFILE);

    let decision = issue(&authority, 1, 3_100, Vec::new());
    let permit = issued_permit(&decision);
    let before = authority
        .inspect_working_set_pressure()
        .expect("pressure before gates");

    let foreign_advisory = WorkingSetReclaimAdvisory {
        profile_id: "task-foreign-tier",
        projected_active_count: 5,
        reclaim_threshold_count: 4,
        reclaim_threshold_ratio: 50,
        max_active_working_set: 8,
    };
    let mismatch = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: nlos_task::plan_working_set_reclaim_execution(&foreign_advisory),
            executed_at_ms: 9_000,
        })
        .expect_err("foreign-tier warrant must fail closed");
    assert!(matches!(
        mismatch,
        TaskStoreError::ReclaimExecutionProfileMismatch {
            advisory_profile_id: "task-foreign-tier",
            authority_profile_id: "task-reclaim-drive-test",
        }
    ));

    let out_of_range = authority
        .drive_working_set_reclaim(WorkingSetReclaimExecutionRequest {
            execution: WorkingSetReclaimExecution {
                execution_sequence: 4,
                phase: ReclaimPhase::Kill,
                advisory: WorkingSetReclaimAdvisory {
                    profile_id: RECLAIM_TEST_PROFILE.profile_id,
                    projected_active_count: 5,
                    reclaim_threshold_count: 4,
                    reclaim_threshold_ratio: 50,
                    max_active_working_set: 8,
                },
            },
            executed_at_ms: 9_000,
        })
        .expect_err("out-of-range sequence must fail closed");
    assert!(matches!(
        out_of_range,
        TaskStoreError::ReclaimExecutionSequenceOutOfRange { sequence: 4 }
    ));

    let after = authority
        .inspect_working_set_pressure()
        .expect("pressure after gates");
    assert_eq!(after, before);
    let inspected = authority
        .inspect_permit(permit.task_id, permit.permit_id)
        .expect("inspect untouched permit");
    assert_eq!(inspected.state, PermitState::Issued);
}
