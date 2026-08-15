//! Acceptance tests for the B-TASK-004 `TaskGroup` slice: acyclic
//! parent/child trees with depth/fanout bounds, content-addressed
//! membership with monotonic generation CAS, immutable Admission/Removal
//! receipts, OPEN-only admission, group-bound attempt registration,
//! structural tree cancellation, the derived ALL/ANY aggregate, the
//! quarantine cap, and the schema v3 → v4 additive migration.
//!
//! Every test maps to one acceptance-gate bullet of the B-TASK-004 task
//! brief (see `docs/evidence/stage-b/b-task-004-task-group.md` §4).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    ArtifactPublicationExpectation, AttemptGroupRegistration, AttemptRegistrationDecision,
    AttemptSpec, AttemptState, CancelRequest, ClosePermitDecision, ClosePermitRequest,
    CompletionMode, EffectPermitDecision, EffectPermitRequest, FailureMode,
    FinalizeArtifactCommitRequest, FinalizeDecision, FinalizeRequestV3, GroupBinding,
    GroupCancelDecision, GroupCancelRequest, GroupMemberRef, GroupMemberType, GroupReceiptKind,
    GroupRegistrationDecision, GroupSpec, GroupState, IssuedPermit, LogicalEffectDescriptor,
    MembershipState, NestedArtifactPublicationReceipt, Outcome, OutcomeRequest,
    PermitClosureOutcome, PermitDecision, PermitRecord, PermitRequest, PermitState,
    PlanArtifactCommitRequest, PlannedEffect, ReceiptOutcome, RecordArtifactPublicationsRequest,
    RemovalDecision, RemoveMemberRequest, SnapshotBundle, SqliteTaskAuthority, TaskGroupId,
    TaskSpec, TaskState, TaskStoreError, artifact_publication_plan_root, empty_effect_history_root,
    empty_group_membership_root, membership_root_of,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ReceiptId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-group-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
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

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn task_id() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

fn task_spec() -> TaskSpec {
    TaskSpec {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

fn register_task(authority: &SqliteTaskAuthority) {
    authority.register_task(task_spec()).expect("register task");
}

fn snapshot_exact(tag: u8, head_seq: u64, root: [u8; 32], fence: u64) -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: root,
        retry_fence_epoch: fence,
    }
}

fn initial_snapshot() -> SnapshotBundle {
    snapshot_exact(0x10, 0, empty_effect_history_root(), 0)
}

fn attempt_spec(seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xc0u8.wrapping_add(seed))),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xa0u8.wrapping_add(seed))),
        registered_at_ms: 2_000,
    }
}

fn group_id(seed: u8) -> TaskGroupId {
    TaskGroupId::from_bytes(bytes(seed))
}

fn root_group_spec(
    seed: u8,
    completion_mode: CompletionMode,
    failure_mode: FailureMode,
    max_children: u64,
    max_depth: u64,
) -> GroupSpec {
    GroupSpec {
        group_id: group_id(seed),
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        parent_group_id: None,
        group_policy_digest: [seed; 32],
        completion_mode,
        failure_mode,
        max_children,
        max_depth,
        resource_group_id: None,
        resource_account_digest: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xd0u8.wrapping_add(seed))),
        registered_at_ms: 1_500,
    }
}

fn child_group_spec(seed: u8, parent: TaskGroupId, max_children: u64, max_depth: u64) -> GroupSpec {
    GroupSpec {
        group_id: group_id(seed),
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        parent_group_id: Some(parent),
        group_policy_digest: [seed; 32],
        completion_mode: CompletionMode::All,
        failure_mode: FailureMode::CollectAll,
        max_children,
        max_depth,
        resource_group_id: None,
        resource_account_digest: None,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xd0u8.wrapping_add(seed))),
        registered_at_ms: 1_600,
    }
}

fn binding_of(authority: &SqliteTaskAuthority, group: TaskGroupId) -> GroupBinding {
    let record = authority.inspect_group(group).expect("inspect group");
    GroupBinding {
        group_id: group,
        expected_membership_generation: record.membership_generation,
        expected_membership_root: record.membership_root,
        expected_group_policy_digest: record.group_policy_digest,
    }
}

fn admit_attempt(
    authority: &SqliteTaskAuthority,
    spec: &AttemptSpec,
    group: TaskGroupId,
) -> AttemptGroupRegistration {
    let binding = binding_of(authority, group);
    authority
        .register_attempt_in_group(*spec, binding)
        .expect("admit attempt")
}

fn permit_request(spec: &AttemptSpec, seed: u8, effects: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0u8.wrapping_add(seed))),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn authorized_artifact_publication_freezes_group_membership() {
    let database = TestDatabase::new("artifact-publication-freeze");
    let authority = database.open();
    register_task(&authority);
    authority
        .register_group(root_group_spec(
            0x01,
            CompletionMode::All,
            FailureMode::CollectAll,
            4,
            1,
        ))
        .expect("register group");
    let attempt_a = attempt_spec(0x21, initial_snapshot());
    admit_attempt(&authority, &attempt_a, group_id(0x01));
    let expectation = ArtifactPublicationExpectation {
        staging_id: bytes(0x31),
        artifact_id: ArtifactId::from_bytes(bytes(0x32)),
        target_revision: 1,
        digest: [0x33; 32],
        size_bytes: 64,
    };
    let write_set_root = artifact_publication_plan_root(&[expectation]).unwrap();
    let permit = issued_permit(
        authority
            .request_commit_permit(PermitRequest {
                task_id: attempt_a.task_id,
                attempt_id: attempt_a.attempt_id,
                attempt_generation: attempt_a.attempt_generation,
                write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes(bytes(0x34)),
                valid_until_ms: 20_000,
                requested_at_ms: 3_000,
            })
            .unwrap(),
    );
    let plan = authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id: attempt_a.task_id,
            attempt_id: attempt_a.attempt_id,
            attempt_generation: attempt_a.attempt_generation,
            permit_id: permit.permit_id,
            expectations: vec![expectation],
            idempotency_key: IdempotencyKey::from_bytes(bytes(0x35)),
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .clone();
    authority
        .authorize_artifact_publication(plan.plan_id, 4_500)
        .unwrap();

    assert!(matches!(
        authority.remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: GroupMemberRef {
                member_type: GroupMemberType::TaskAttempt,
                member_id: attempt_a.attempt_id.into_bytes(),
                member_generation: attempt_a.attempt_generation,
            },
            removed_at_ms: 5_000,
        }),
        Err(TaskStoreError::GroupPublicationInFlight)
    ));
    let attempt_b = attempt_spec(0x22, initial_snapshot());
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, binding_of(&authority, group_id(0x01))),
        Err(TaskStoreError::GroupPublicationInFlight)
    ));
    assert_eq!(
        authority.list_group_members(group_id(0x01)).unwrap().len(),
        1
    );

    authority
        .record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![NestedArtifactPublicationReceipt {
                receipt_id: ReceiptId::from_bytes(bytes(0x36)),
                staging_id: expectation.staging_id,
                artifact_id: expectation.artifact_id,
                revision: expectation.target_revision,
                digest: expectation.digest,
                size_bytes: expectation.size_bytes,
                task_id: attempt_a.task_id,
                permit_id: permit.permit_id,
                write_set_root,
                prior_head_revision: 0,
                prior_head_digest: None,
                new_head_revision: 1,
                new_head_digest: expectation.digest,
                created_at_ms: 5_500,
            }],
            observed_at_ms: 6_000,
        })
        .unwrap();
    authority
        .finalize_artifact_commit(FinalizeArtifactCommitRequest {
            plan_id: plan.plan_id,
            finalized_at_ms: 6_500,
        })
        .unwrap();
    authority
        .register_attempt_in_group(attempt_b, binding_of(&authority, group_id(0x01)))
        .expect("finalize releases membership freeze");
    assert_eq!(
        authority.list_group_members(group_id(0x01)).unwrap().len(),
        2
    );
}

/// Drives an attempt through a no-effect permit to `Committed`; returns
/// the durable receipt (the new head) so callers can chain snapshots.
fn commit_no_effect(authority: &SqliteTaskAuthority, spec: &AttemptSpec, seed: u8) -> [u8; 32] {
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(spec, seed, vec![]))
            .expect("permit"),
    );
    let new_root = [seed; 32];
    match authority
        .finalize_commit(nlos_task::FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: new_root,
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        })
        .expect("finalize")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::Committed);
        }
        FinalizeDecision::Replayed(_) => panic!("expected fresh commit"),
    }
    new_root
}

fn descriptor(stable_action_slot: u64) -> LogicalEffectDescriptor {
    LogicalEffectDescriptor {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

fn planned(stable_action_slot: u64, required: bool) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(stable_action_slot),
        required,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(issued) => *issued,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected effect permit Issued, got {other:?}")
        }
    }
}

/// Drives the single declared slot of `permit` to `EffectClosed`.
fn close_slot(authority: &SqliteTaskAuthority, spec: &AttemptSpec, permit: &PermitRecord) {
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(EffectPermitRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes(bytes(0xe0)),
                valid_until_ms: 9_999,
                requested_at_ms: 4_000,
            })
            .expect("effect permit"),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_permit_id: issued.effect_permit_id,
            dispatch_token: issued.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .expect("dispatch");
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
            recorded_at_ms: 6_000,
        })
        .expect("close slot");
}

/// Drives an attempt to `Failed` via `FAILED_AFTER_EFFECT`
/// (`[TASK-RETRY-EFFECT-001]`): a required slot closes with an effect
/// and no success proof is presented.
fn fail_attempt(authority: &SqliteTaskAuthority, spec: &AttemptSpec, seed: u8) {
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(spec, seed, vec![planned(0, true)]))
            .expect("permit"),
    );
    close_slot(authority, spec, &permit);
    match authority
        .finalize_commit_v3(nlos_task::FinalizeRequestV3 {
            base: nlos_task::FinalizeRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: permit.permit_id,
                new_effect_history_root: [0u8; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: 7_000,
            },
            required_satisfaction: vec![],
            fenced_participant_digest: [0xf1; 32],
        })
        .expect("finalize v3")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::FailedAfterEffect);
        }
        FinalizeDecision::Replayed(_) => panic!("expected fresh failure"),
    }
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::Failed
    );
}

/// Drives an attempt into the quarantine tombstone (`[TASK-EFFECT-003]`):
/// one slot `EffectUnknown`, then `close_permit` quarantines the permit.
fn quarantine_attempt(authority: &SqliteTaskAuthority, spec: &AttemptSpec, seed: u8) {
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(spec, seed, vec![planned(0, false)]))
            .expect("permit"),
    );
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(EffectPermitRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes(bytes(0xe1)),
                valid_until_ms: 9_999,
                requested_at_ms: 4_000,
            })
            .expect("effect permit"),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_permit_id: issued.effect_permit_id,
            dispatch_token: issued.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .expect("dispatch");
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Unknown {
                uncertainty_digest: [0xbb; 32],
            },
            recorded_at_ms: 6_000,
        })
        .expect("unknown slot");
    match authority
        .close_permit(ClosePermitRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            outcome: PermitClosureOutcome::CancelledBeforeEffect,
            fenced_participant_digest: [0xf2; 32],
            closed_at_ms: 7_000,
        })
        .expect("close permit")
    {
        ClosePermitDecision::Quarantined(_) => {}
        other => panic!("expected Quarantined, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined
    );
}

/// Bullet: `TaskGroup` registration forms an acyclic parent/child tree —
/// self-parent is refused, a missing parent is refused (which makes
/// A→B→A unconstructible: parents must pre-exist and parent bindings
/// are immutable), a cross-task parent is refused, one root per task,
/// idempotent replay, and reserved modes fail closed.
#[test]
fn group_registration_enforces_acyclic_tree_and_single_root() {
    let database = TestDatabase::new("registration");
    let authority = database.open();
    register_task(&authority);

    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    assert_eq!(
        authority.register_group(root).expect("root"),
        GroupRegistrationDecision::Created(group_id(0x01))
    );
    // Idempotent replay of the exact same spec.
    assert_eq!(
        authority.register_group(root).expect("replay"),
        GroupRegistrationDecision::Existing(group_id(0x01))
    );
    // Same ID, different bytes: fail-closed.
    let mut conflicting = root;
    conflicting.group_policy_digest = [0xff; 32];
    assert!(matches!(
        authority.register_group(conflicting),
        Err(TaskStoreError::DuplicateGroup)
    ));
    // Exactly one root per task.
    let second_root = root_group_spec(0x02, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    assert!(matches!(
        authority.register_group(second_root),
        Err(TaskStoreError::DuplicateGroup)
    ));
    // Self-parent refused.
    let mut self_parent = child_group_spec(0x03, group_id(0x03), 4, 2);
    self_parent.parent_group_id = Some(group_id(0x03));
    assert!(matches!(
        authority.register_group(self_parent),
        Err(TaskStoreError::GroupCycle)
    ));
    // Missing parent refused (A→B→A is unconstructible: B must exist
    // before A can bind to it, and A already exists when B binds back).
    let orphan = child_group_spec(0x04, group_id(0xee), 4, 2);
    assert!(matches!(
        authority.register_group(orphan),
        Err(TaskStoreError::GroupNotFound)
    ));
    // Parent in a different task refused.
    let other_task = TaskSpec {
        task_id: TaskId::from_bytes(bytes(0x02)),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_100,
    };
    authority.register_task(other_task).expect("other task");
    let mut cross_task = child_group_spec(0x05, group_id(0x01), 4, 2);
    cross_task.task_id = TaskId::from_bytes(bytes(0x02));
    assert!(matches!(
        authority.register_group(cross_task),
        Err(TaskStoreError::GroupNotFound)
    ));
    // Unknown task refused.
    let mut unknown_task =
        root_group_spec(0x06, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    unknown_task.task_id = TaskId::from_bytes(bytes(0x7f));
    assert!(matches!(
        authority.register_group(unknown_task),
        Err(TaskStoreError::TaskNotFound)
    ));
    // Task generation mismatch refused.
    let mut wrong_generation =
        root_group_spec(0x07, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    wrong_generation.task_generation = Generation::new(std::num::NonZeroU64::new(2).expect("nz"));
    assert!(matches!(
        authority.register_group(wrong_generation),
        Err(TaskStoreError::InvalidGeneration)
    ));
    // Reserved modes fail closed.
    for mut reserved in [
        root_group_spec(0x08, CompletionMode::Quorum, FailureMode::CollectAll, 8, 4),
        root_group_spec(0x08, CompletionMode::Reduce, FailureMode::CollectAll, 8, 4),
        root_group_spec(0x08, CompletionMode::All, FailureMode::BestEffort, 8, 4),
    ] {
        reserved.task_id = TaskId::from_bytes(bytes(0x02));
        assert!(matches!(
            authority.register_group(reserved),
            Err(TaskStoreError::UnsupportedGroupMode)
        ));
    }
    // A cancelled task admits no new groups.
    authority
        .cancel_task(CancelRequest {
            task_id: TaskId::from_bytes(bytes(0x02)),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc1)),
            requested_at_ms: 8_000,
        })
        .expect("cancel task");
    let mut on_cancelled =
        root_group_spec(0x09, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    on_cancelled.task_id = TaskId::from_bytes(bytes(0x02));
    assert!(matches!(
        authority.register_group(on_cancelled),
        Err(TaskStoreError::TaskCancelled)
    ));
}

/// Bullet: `max_depth` / `max_children` are enforced fail-closed;
/// `max_depth` binds across the WHOLE ancestor chain, and removal
/// frees a fanout slot.
#[test]
fn depth_and_fanout_bounds_fail_closed() {
    let database = TestDatabase::new("bounds");
    let authority = database.open();
    register_task(&authority);
    // root: max_depth 1 (children only), max_children 2.
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 2, 1);
    authority.register_group(root).expect("root");
    // Child at depth 1 with its own generous max_depth: the ROOT bound
    // still fences the grandchild (ancestor-chain enforcement).
    let child = child_group_spec(0x02, group_id(0x01), 4, 5);
    authority.register_group(child).expect("child");
    let grandchild = child_group_spec(0x03, group_id(0x02), 4, 5);
    assert!(matches!(
        authority.register_group(grandchild),
        Err(TaskStoreError::GroupDepthExceeded)
    ));
    // Fanout: root holds {child}; one attempt member fills the second slot.
    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority, &attempt_a, group_id(0x01));
    let overflow = child_group_spec(0x04, group_id(0x01), 4, 1);
    assert!(matches!(
        authority.register_group(overflow),
        Err(TaskStoreError::GroupFanoutExceeded)
    ));
    // Removal frees the slot.
    let removed = authority
        .remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: GroupMemberRef {
                member_type: GroupMemberType::TaskAttempt,
                member_id: attempt_a.attempt_id.into_bytes(),
                member_generation: attempt_a.attempt_generation,
            },
            removed_at_ms: 8_500,
        })
        .expect("remove");
    assert!(matches!(removed, RemovalDecision::Removed(_)));
    authority
        .register_group(overflow)
        .expect("fits after removal");
}

/// Bullet: membership changes advance a strictly monotonic generation
/// CAS and recompute the content-addressed root over the canonical
/// ACTIVE member set; receipts pin the generation/root they produced.
#[test]
fn membership_generation_cas_is_monotonic_and_root_recomputes() {
    let database = TestDatabase::new("membership-cas");
    let authority = database.open();
    register_task(&authority);
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(root).expect("root");

    let record = authority.inspect_group(group_id(0x01)).expect("group");
    assert_eq!(record.membership_generation, 0);
    assert_eq!(record.membership_root, empty_group_membership_root());

    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    let admission_a = admit_attempt(&authority, &attempt_a, group_id(0x01));
    let attempt_b = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority, &attempt_b, group_id(0x01));
    let child = child_group_spec(0x02, group_id(0x01), 4, 2);
    authority.register_group(child).expect("child");

    let record = authority.inspect_group(group_id(0x01)).expect("group");
    assert_eq!(record.membership_generation, 3);
    let members = authority
        .list_group_members(group_id(0x01))
        .expect("members");
    assert_eq!(members.len(), 3);
    assert_eq!(record.membership_root, membership_root_of(&members));
    // The admission receipt pins the exact generation/root it produced.
    let receipts = authority
        .list_group_receipts(group_id(0x01))
        .expect("receipts");
    assert_eq!(receipts.len(), 3);
    assert_eq!(admission_a.admission_receipt_id, receipts[0].receipt_id);
    assert_eq!(receipts[0].membership_generation_after, 1);
    assert_eq!(receipts[2].membership_generation_after, 3);
    assert_eq!(receipts[2].membership_root_after, record.membership_root);
    let root_with_three = record.membership_root;

    // Removal advances the generation again and recomputes over the
    // remaining ACTIVE set.
    authority
        .remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: GroupMemberRef {
                member_type: GroupMemberType::TaskAttempt,
                member_id: attempt_b.attempt_id.into_bytes(),
                member_generation: attempt_b.attempt_generation,
            },
            removed_at_ms: 8_500,
        })
        .expect("remove");
    let record = authority.inspect_group(group_id(0x01)).expect("group");
    assert_eq!(record.membership_generation, 4);
    let active: Vec<_> = authority
        .list_group_members(group_id(0x01))
        .expect("members")
        .into_iter()
        .filter(|member| member.membership_state == MembershipState::Active)
        .collect();
    assert_eq!(active.len(), 2);
    assert_eq!(record.membership_root, membership_root_of(&active));
    assert_ne!(record.membership_root, root_with_three);
    let receipts = authority
        .list_group_receipts(group_id(0x01))
        .expect("receipts");
    assert_eq!(receipts.len(), 4);
    assert_eq!(receipts[3].kind, GroupReceiptKind::Removal);
    assert_eq!(receipts[3].membership_generation_after, 4);
    assert_eq!(receipts[3].membership_root_after, record.membership_root);
}

/// Bullet: Admission/Removal receipts are immutable (SQL trigger),
/// per-member (type/id/generation/ControlDomain placeholder), and
/// replay-safe: re-admission replay returns the original receipt,
/// removal replay returns the original removal receipt, and identity
/// drift fails closed.
#[test]
fn admission_and_removal_receipts_are_immutable_and_replay_safe() {
    let database = TestDatabase::new("receipts");
    let authority = database.open();
    register_task(&authority);
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(root).expect("root");
    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    let binding = binding_of(&authority, group_id(0x01));
    let admission = authority
        .register_attempt_in_group(attempt_a, binding)
        .expect("admit");

    // Replay: same key + same bytes + same group returns the original
    // admission, even though the presented expected generation is now
    // stale (replay never re-checks drift).
    let replay = authority
        .register_attempt_in_group(attempt_a, binding)
        .expect("replay");
    assert!(matches!(
        replay.decision,
        AttemptRegistrationDecision::Existing(_)
    ));
    assert_eq!(replay.admission_receipt_id, admission.admission_receipt_id);
    assert_eq!(replay.binding, admission.binding);
    // Same key, different attempt bytes: fail-closed.
    let mut conflicting = attempt_a;
    conflicting.snapshot = snapshot_exact(0x11, 0, empty_effect_history_root(), 0);
    assert!(matches!(
        authority.register_attempt_in_group(conflicting, binding),
        Err(TaskStoreError::IdempotencyConflict)
    ));

    // The receipt rows the member binding.
    let members = authority
        .list_group_members(group_id(0x01))
        .expect("members");
    assert_eq!(members.len(), 1);
    let member = &members[0];
    assert_eq!(member.member_type, GroupMemberType::TaskAttempt);
    assert_eq!(member.member_id, attempt_a.attempt_id.into_bytes());
    assert_eq!(member.member_generation, attempt_a.attempt_generation);
    assert_eq!(member.control_domain_id, None);
    assert!(!member.detached);
    assert_eq!(member.admission_receipt_id, admission.admission_receipt_id);

    // Receipts are immutable on disk.
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw");
        assert!(
            connection
                .execute(
                    "UPDATE task_group_admission_receipts SET created_at_ms = created_at_ms",
                    []
                )
                .is_err(),
            "admission receipt trigger enforces"
        );
    }

    // Removal produces its own receipt; replay returns the original.
    let member_ref = GroupMemberRef {
        member_type: GroupMemberType::TaskAttempt,
        member_id: attempt_a.attempt_id.into_bytes(),
        member_generation: attempt_a.attempt_generation,
    };
    let removal = authority
        .remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: member_ref,
            removed_at_ms: 8_500,
        })
        .expect("remove");
    let RemovalDecision::Removed(removal_receipt) = removal else {
        panic!("expected fresh removal");
    };
    assert_eq!(removal_receipt.kind, GroupReceiptKind::Removal);
    let replayed = authority
        .remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: member_ref,
            removed_at_ms: 8_600,
        })
        .expect("removal replay");
    let RemovalDecision::Replayed(original) = replayed else {
        panic!("expected replayed removal");
    };
    assert_eq!(original.receipt_id, removal_receipt.receipt_id);
    // Generation drift on removal fails closed.
    let mut drifted = member_ref;
    drifted.member_generation = Generation::new(std::num::NonZeroU64::new(2).expect("nz"));
    assert!(matches!(
        authority.remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: drifted,
            removed_at_ms: 8_700,
        }),
        Err(TaskStoreError::MembershipConflict)
    ));
    // Unknown member fails closed.
    let unknown = GroupMemberRef {
        member_type: GroupMemberType::TaskAttempt,
        member_id: bytes(0xfe),
        member_generation: Generation::INITIAL,
    };
    assert!(matches!(
        authority.remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: unknown,
            removed_at_ms: 8_800,
        }),
        Err(TaskStoreError::GroupMemberNotFound)
    ));
}

/// Bullet: only OPEN admits new children — SEALED refuses child groups,
/// attempt members, and removals; sealing is replay-safe; the sealed
/// membership set is frozen for the aggregate.
#[test]
fn sealed_group_rejects_new_children_and_removals() {
    let database = TestDatabase::new("sealed");
    let authority = database.open();
    register_task(&authority);
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(root).expect("root");
    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority, &attempt_a, group_id(0x01));

    let sealed = authority.seal_group(group_id(0x01), 8_000).expect("seal");
    assert_eq!(sealed.state, GroupState::Sealed);
    assert_eq!(sealed.state_seq, 2);
    // Seal replay returns the sealed record unchanged.
    let replay = authority.seal_group(group_id(0x01), 8_100).expect("replay");
    assert_eq!(replay.state, GroupState::Sealed);
    assert_eq!(replay.state_seq, 2);

    let child = child_group_spec(0x02, group_id(0x01), 4, 2);
    assert!(matches!(
        authority.register_group(child),
        Err(TaskStoreError::GroupSealed)
    ));
    let attempt_b = attempt_spec(0x0b, initial_snapshot());
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, binding_of(&authority, group_id(0x01))),
        Err(TaskStoreError::GroupSealed)
    ));
    assert!(matches!(
        authority.remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: GroupMemberRef {
                member_type: GroupMemberType::TaskAttempt,
                member_id: attempt_a.attempt_id.into_bytes(),
                member_generation: attempt_a.attempt_generation,
            },
            removed_at_ms: 8_200,
        }),
        Err(TaskStoreError::GroupSealed)
    ));
    // Membership is frozen at the sealed set.
    let record = authority.inspect_group(group_id(0x01)).expect("group");
    assert_eq!(record.membership_generation, 1);
    assert_eq!(
        authority
            .list_group_members(group_id(0x01))
            .expect("members")
            .len(),
        1
    );
}

/// Bullet: attempt registration binds the group membership
/// generation/root + policy digest bit-for-bit; drift fails closed with
/// typed errors; the persisted binding records the post-admission
/// membership position. Ungrouped attempts keep B-TASK-001 behavior.
#[test]
fn attempt_group_binding_drift_fails_closed() {
    let database = TestDatabase::new("binding-drift");
    let authority = database.open();
    register_task(&authority);
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(root).expect("root");

    // Admit one attempt: binding records generation 1 (post-admission).
    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    let admission = admit_attempt(&authority, &attempt_a, group_id(0x01));
    assert_eq!(admission.binding.membership_generation, 1);
    let stored = authority
        .inspect_attempt_group_binding(attempt_a.attempt_id)
        .expect("binding")
        .expect("present");
    assert_eq!(stored, admission.binding);

    // Stale generation: typed conflict carrying both values.
    let attempt_b = attempt_spec(0x0b, initial_snapshot());
    let mut stale = binding_of(&authority, group_id(0x01));
    stale.expected_membership_generation = 0;
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, stale),
        Err(TaskStoreError::StaleMembershipGeneration {
            expected: 0,
            current: 1
        })
    ));
    // Root drift fails closed.
    let mut root_drift = binding_of(&authority, group_id(0x01));
    root_drift.expected_membership_root = [0xee; 32];
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, root_drift),
        Err(TaskStoreError::MembershipConflict)
    ));
    // Policy digest drift fails closed.
    let mut policy_drift = binding_of(&authority, group_id(0x01));
    policy_drift.expected_group_policy_digest = [0xdd; 32];
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, policy_drift),
        Err(TaskStoreError::MembershipConflict)
    ));
    // Binding to a group of another task / unknown group fails closed.
    let mut foreign = binding_of(&authority, group_id(0x01));
    foreign.group_id = group_id(0xee);
    assert!(matches!(
        authority.register_attempt_in_group(attempt_b, foreign),
        Err(TaskStoreError::GroupNotFound)
    ));

    // Ungrouped attempts keep the B-TASK-001 behavior and carry no
    // binding.
    let attempt_u = attempt_spec(0x0c, initial_snapshot());
    authority
        .register_attempt(attempt_u)
        .expect("ungrouped attempt");
    assert!(
        authority
            .inspect_attempt_group_binding(attempt_u.attempt_id)
            .expect("binding lookup")
            .is_none()
    );
}

/// B-TASK-005: a grouped write set snapshots the group's current
/// generation/root/policy into the `CommitPermit`, terminalization
/// revalidates the same bytes, and `TaskCommitReceipt` preserves them across
/// restart. Ungrouped permits remain bit-compatible with `None`.
#[test]
fn permit_and_commit_receipt_bind_group_membership_across_restart() {
    let database = TestDatabase::new("commit-group-binding");
    let authority = database.open();
    register_task(&authority);
    authority
        .register_group(root_group_spec(
            0x01,
            CompletionMode::All,
            FailureMode::CollectAll,
            8,
            4,
        ))
        .expect("root");
    let attempt = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority, &attempt, group_id(0x01));
    let group = authority.inspect_group(group_id(0x01)).expect("group");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&attempt, 0x21, vec![]))
            .expect("permit"),
    );
    let binding = permit.group_binding.expect("group binding");
    assert_eq!(binding.group_id, group.group_id);
    assert_eq!(binding.membership_generation, group.membership_generation);
    assert_eq!(binding.membership_root, group.membership_root);
    assert_eq!(binding.group_policy_digest, group.group_policy_digest);

    let receipt = match authority
        .finalize_commit(nlos_task::FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: [0x31; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        })
        .expect("finalize")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        FinalizeDecision::Replayed(_) => panic!("expected fresh commit"),
    };
    assert_eq!(receipt.group_binding, Some(binding));
    let receipt_id = receipt.receipt_id;
    drop(authority);

    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit after restart")
            .group_binding,
        Some(binding)
    );
    assert_eq!(
        reopened
            .inspect_receipt(task_id(), receipt_id)
            .expect("receipt after restart")
            .group_binding,
        Some(binding)
    );
}

/// B-TASK-005: changing membership after permit issuance cannot be hidden
/// by finalization; the `TaskHead` and issued permit remain untouched.
#[test]
fn membership_drift_after_permit_fails_terminalization_closed() {
    let database = TestDatabase::new("commit-group-binding-drift");
    let authority = database.open();
    register_task(&authority);
    authority
        .register_group(root_group_spec(
            0x01,
            CompletionMode::All,
            FailureMode::CollectAll,
            8,
            4,
        ))
        .expect("root");
    let winner = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority, &winner, group_id(0x01));
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&winner, 0x22, vec![planned(0, false)]))
            .expect("permit"),
    );
    let bound = permit.group_binding.expect("binding");

    // A second admission advances generation/root while the permit is
    // live. The old permit remains durable but cannot terminalize under a
    // membership position it no longer owns.
    let late = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority, &late, group_id(0x01));
    let current = authority.inspect_group(group_id(0x01)).expect("group");
    assert!(current.membership_generation > bound.membership_generation);
    assert_ne!(current.membership_root, bound.membership_root);
    assert!(matches!(
        authority.request_effect_permit(EffectPermitRequest {
            task_id: winner.task_id,
            attempt_id: winner.attempt_id,
            attempt_generation: winner.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xe2)),
            valid_until_ms: 9_999,
            requested_at_ms: 6_900,
        }),
        Err(TaskStoreError::MembershipConflict)
    ));
    assert!(matches!(
        authority.finalize_commit(nlos_task::FinalizeRequest {
            task_id: winner.task_id,
            attempt_id: winner.attempt_id,
            attempt_generation: winner.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: [0x32; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_100,
        }),
        Err(TaskStoreError::MembershipConflict)
    ));
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.active_permit, Some(permit.permit_id));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        nlos_task::PermitState::Issued
    );
}

/// B-TASK-005 migration gate: a structurally valid v4 database upgrades
/// transactionally to v5; pre-v5 permits remain explicitly ungrouped and
/// can still finalize with an ungrouped receipt.
#[test]
fn schema_v4_upgrades_to_v5_without_inventing_group_bindings() {
    let database = TestDatabase::new("migration-v5");
    let authority = database.open();
    register_task(&authority);
    let attempt = attempt_spec(0x0a, initial_snapshot());
    authority.register_attempt(attempt).expect("attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&attempt, 0x23, vec![]))
            .expect("permit"),
    );
    assert_eq!(permit.group_binding, None);
    drop(authority);

    // Reconstruct the v4 shape by removing exactly the additive v5
    // columns, then let the production migration run again.
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw v5 database");
        connection
            .execute_batch(
                "DROP TABLE task_snapshot_checkpoint_receipts;
                 DROP TABLE task_snapshot_receipts;
                 ALTER TABLE task_attempts DROP COLUMN snapshot_receipt_id;
                 DROP TABLE task_artifact_recovery_alert_receipts;
                 DROP TABLE task_artifact_recovery;
                 DROP TABLE task_artifact_publication_receipts;
                 DROP TABLE task_artifact_publication_expectations;
                 DROP TABLE task_artifact_commit_plans;
                 ALTER TABLE commit_permits DROP COLUMN group_policy_digest;
                 ALTER TABLE commit_permits DROP COLUMN membership_root;
                 ALTER TABLE commit_permits DROP COLUMN membership_generation;
                 ALTER TABLE commit_permits DROP COLUMN group_id;
                 ALTER TABLE task_receipts DROP COLUMN group_policy_digest;
                 ALTER TABLE task_receipts DROP COLUMN membership_root;
                 ALTER TABLE task_receipts DROP COLUMN membership_generation;
                 ALTER TABLE task_receipts DROP COLUMN group_id;
                 PRAGMA user_version = 4;",
            )
            .expect("restore v4 shape");
    }

    let migrated = database.open();
    let stored = migrated
        .inspect_permit(task_id(), permit.permit_id)
        .expect("migrated permit");
    assert_eq!(stored.group_binding, None);
    let receipt = match migrated
        .finalize_commit(nlos_task::FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: [0x33; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_200,
        })
        .expect("finalize migrated permit")
    {
        FinalizeDecision::Committed(receipt) => *receipt,
        FinalizeDecision::Replayed(_) => panic!("expected fresh commit"),
    };
    assert_eq!(receipt.group_binding, None);
    let connection = rusqlite::Connection::open(&database.path).expect("raw migrated database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 24);
}

/// Builds the cancellation fixture: root group + child group with an
/// open pre-permit member each, a terminal-success member, a
/// permit-holding member, and an ungrouped attempt of the same task.
struct CancelFixture {
    attempt_a1: AttemptSpec,
    attempt_a2: AttemptSpec,
    attempt_a3: AttemptSpec,
    attempt_a6: AttemptSpec,
    attempt_a4: AttemptSpec,
    permit_a6: PermitRecord,
}

fn setup_cancel_fixture(authority: &SqliteTaskAuthority) -> CancelFixture {
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(root).expect("root");
    let child = child_group_spec(0x02, group_id(0x01), 8, 3);
    authority.register_group(child).expect("child");

    // a1: open pre-permit member of the root group.
    let attempt_a1 = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(authority, &attempt_a1, group_id(0x01));
    // a2: open pre-permit member of the child group.
    let attempt_a2 = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(authority, &attempt_a2, group_id(0x02));
    // a3: terminal-success member of the child group.
    let attempt_a3 = attempt_spec(0x0c, initial_snapshot());
    admit_attempt(authority, &attempt_a3, group_id(0x02));
    let new_root = commit_no_effect(authority, &attempt_a3, 0x31);
    // a6: permit-holding member of the child group (not finalized).
    let attempt_a6 = attempt_spec(0x0d, snapshot_exact(0x12, 1, new_root, 0));
    admit_attempt(authority, &attempt_a6, group_id(0x02));
    let permit_a6 = issued_permit(
        authority
            .request_commit_permit(permit_request(&attempt_a6, 0x32, vec![]))
            .expect("permit a6"),
    );
    // a4: ungrouped attempt of the same task; unaffected by group cancel.
    let attempt_a4 = attempt_spec(0x0e, snapshot_exact(0x12, 1, new_root, 0));
    authority.register_attempt(attempt_a4).expect("a4");
    CancelFixture {
        attempt_a1,
        attempt_a2,
        attempt_a3,
        attempt_a6,
        attempt_a4,
        permit_a6,
    }
}

fn cancel_root(authority: &SqliteTaskAuthority) -> GroupCancelDecision {
    authority
        .cancel_group(GroupCancelRequest {
            group_id: group_id(0x01),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc5)),
            requested_at_ms: 9_000,
        })
        .expect("cancel group")
}

/// Bullet: parent `cancel_epoch++` propagates IN ONE TRANSACTION to all
/// non-terminal descendants (child groups + pre-permit attempts with
/// closure receipts, head unchanged); terminal children stay untouched;
/// permit-holding attempts stay untouched (permit-first linearization);
/// ungrouped attempts of the same task are unaffected.
#[test]
fn group_cancel_propagates_to_non_terminal_descendants() {
    let database = TestDatabase::new("cancel-propagation");
    let authority = database.open();
    register_task(&authority);
    let fixture = setup_cancel_fixture(&authority);
    let attempt_a1 = fixture.attempt_a1;
    let attempt_a2 = fixture.attempt_a2;
    let attempt_a3 = fixture.attempt_a3;
    let attempt_a6 = fixture.attempt_a6;
    let attempt_a4 = fixture.attempt_a4;

    let head_before = authority.inspect_task(task_id()).expect("head");
    let decision = cancel_root(&authority);
    let GroupCancelDecision::Applied {
        cancel_epoch,
        closed_attempts,
        cancelled_groups,
    } = decision
    else {
        panic!("expected Applied, got {decision:?}");
    };
    assert_eq!(cancel_epoch, 1);
    assert_eq!(cancelled_groups, vec![group_id(0x02)]);
    let closed: Vec<_> = closed_attempts
        .iter()
        .map(|closed| closed.attempt_id)
        .collect();
    assert_eq!(closed.len(), 2);
    assert!(closed.contains(&attempt_a1.attempt_id));
    assert!(closed.contains(&attempt_a2.attempt_id));

    // Pre-permit members closed with closure receipts; head unchanged.
    for closed_attempt in &closed_attempts {
        let attempt = authority
            .inspect_attempt(task_id(), closed_attempt.attempt_id)
            .expect("attempt");
        assert_eq!(attempt.state, AttemptState::Cancelled);
        let receipt = authority
            .inspect_receipt(task_id(), closed_attempt.receipt_id)
            .expect("receipt");
        assert_eq!(receipt.outcome, ReceiptOutcome::CancelledBeforeEffect);
        assert_eq!(receipt.prior_head_commit_seq, receipt.new_head_commit_seq);
        assert_eq!(receipt.new_head_commit_seq, head_before.head_commit_seq);
    }
    // Terminal child untouched; permit holder untouched; ungrouped
    // attempt untouched.
    assert_eq!(
        authority
            .inspect_attempt(task_id(), attempt_a3.attempt_id)
            .expect("a3")
            .state,
        AttemptState::Committed
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), attempt_a6.attempt_id)
            .expect("a6")
            .state,
        AttemptState::CommitPermitted
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), attempt_a4.attempt_id)
            .expect("a4")
            .state,
        AttemptState::Created
    );
    // Both groups landed CANCELLED with their epochs advanced once.
    let child_record = authority.inspect_group(group_id(0x02)).expect("child");
    assert_eq!(child_record.state, GroupState::Cancelled);
    assert_eq!(child_record.cancel_epoch, 1);
    let root_record = authority.inspect_group(group_id(0x01)).expect("root");
    assert_eq!(root_record.state, GroupState::Cancelled);
    assert_eq!(root_record.cancel_epoch, 1);
    // The task itself is NOT cancelled and the head did not move.
    let head_after = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head_after.state, TaskState::Active);
    assert_eq!(head_after.head_commit_seq, head_before.head_commit_seq);
    assert_eq!(head_after.cancel_epoch, 0);
}

/// Bullet (continued): group-cancel replay does not re-increment the
/// epoch, a different key observes `AlreadyCancelled`, the outstanding
/// permit still finalizes after the group cancel (permit-first
/// linearization, `[TASK-CANCEL-003]`), and cancelling a terminal group
/// fails closed.
#[test]
fn group_cancel_replay_and_permit_first_finalization() {
    let database = TestDatabase::new("cancel-replay");
    let authority = database.open();
    register_task(&authority);
    let fixture = setup_cancel_fixture(&authority);
    cancel_root(&authority);

    // The outstanding permit still finalizes.
    authority
        .finalize_commit(nlos_task::FinalizeRequest {
            task_id: task_id(),
            attempt_id: fixture.attempt_a6.attempt_id,
            attempt_generation: fixture.attempt_a6.attempt_generation,
            permit_id: fixture.permit_a6.permit_id,
            new_effect_history_root: [0x33; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 9_500,
        })
        .expect("a6 finalizes after group cancel");

    // Replay does not re-increment; a different key observes
    // AlreadyCancelled.
    assert!(matches!(
        authority.cancel_group(GroupCancelRequest {
            group_id: group_id(0x01),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc5)),
            requested_at_ms: 9_600,
        }),
        Ok(GroupCancelDecision::Replayed { cancel_epoch: 1 })
    ));
    assert!(matches!(
        authority.cancel_group(GroupCancelRequest {
            group_id: group_id(0x01),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc6)),
            requested_at_ms: 9_700,
        }),
        Ok(GroupCancelDecision::AlreadyCancelled { cancel_epoch: 1 })
    ));
    assert_eq!(
        authority
            .inspect_group(group_id(0x01))
            .expect("root")
            .cancel_epoch,
        1
    );

    // Cancelling an already-terminal group with no committed cancel
    // record fails closed.
    let database2 = TestDatabase::new("cancel-terminal");
    let authority2 = database2.open();
    register_task(&authority2);
    let group = root_group_spec(0x01, CompletionMode::Any, FailureMode::CollectAll, 8, 4);
    authority2.register_group(group).expect("group");
    let attempt_x = attempt_spec(0x1a, initial_snapshot());
    admit_attempt(&authority2, &attempt_x, group_id(0x01));
    commit_no_effect(&authority2, &attempt_x, 0x52);
    let record = authority2
        .refresh_group_aggregate(group_id(0x01), 8_000)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Completed);
    assert!(matches!(
        authority2.cancel_group(GroupCancelRequest {
            group_id: group_id(0x01),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc8)),
            requested_at_ms: 9_800,
        }),
        Err(TaskStoreError::InvalidGroupState {
            state: GroupState::Completed
        })
    ));
}

/// Bullet: the aggregate is a DERIVED view of child terminal states —
/// ALL completes only when every active child is terminal-success; ANY
/// completes on the first child success. A terminal group is stable
/// under further refreshes; an empty sealed ALL group completes
/// vacuously (documented placeholder semantics).
#[test]
fn aggregate_all_and_any_modes() {
    let database = TestDatabase::new("aggregate-all");
    let authority = database.open();
    register_task(&authority);
    // ALL group: completes only when EVERY active child is
    // terminal-success. (Attempts freeze their head binding at
    // registration, so the second member is admitted after the first
    // commit moved the head.)
    let all = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority.register_group(all).expect("all group");
    let attempt_a = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority, &attempt_a, group_id(0x01));

    // No terminal child: refresh changes nothing.
    let record = authority
        .refresh_group_aggregate(group_id(0x01), 8_000)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Open);
    assert_eq!(record.state_seq, 1);
    let new_root = commit_no_effect(&authority, &attempt_a, 0x41);
    // One of one committed so far, then a second member admitted: one
    // non-terminal child blocks ALL.
    let attempt_b = attempt_spec(0x0b, snapshot_exact(0x13, 1, new_root, 0));
    admit_attempt(&authority, &attempt_b, group_id(0x01));
    let record = authority
        .refresh_group_aggregate(group_id(0x01), 8_100)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Open);
    // Both committed: ALL completes.
    commit_no_effect(&authority, &attempt_b, 0x42);
    let record = authority
        .refresh_group_aggregate(group_id(0x01), 8_200)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Completed);
    assert!(record.state_seq > 1);
    // Terminal groups are stable under refresh.
    let stable = authority
        .refresh_group_aggregate(group_id(0x01), 8_300)
        .expect("refresh");
    assert_eq!(stable.state, GroupState::Completed);
    assert_eq!(stable.state_seq, record.state_seq);

    // A stale-snapshot member that terminally loses the permit CAS
    // (Conflicted, Cancelled-class) blocks COMPLETED: ALL derives
    // PARTIAL when some children are not successful.
    let database_mixed = TestDatabase::new("aggregate-all-mixed");
    let authority_mixed = database_mixed.open();
    register_task(&authority_mixed);
    let mixed = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority_mixed.register_group(mixed).expect("mixed group");
    let success = attempt_spec(0x0a, initial_snapshot());
    let stale_member = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority_mixed, &success, group_id(0x01));
    admit_attempt(&authority_mixed, &stale_member, group_id(0x01));
    commit_no_effect(&authority_mixed, &success, 0x43);
    let decision = authority_mixed
        .request_commit_permit(permit_request(&stale_member, 0x44, vec![]))
        .expect("stale member competes");
    assert!(matches!(decision, PermitDecision::Conflicted { .. }));
    let record = authority_mixed
        .refresh_group_aggregate(group_id(0x01), 8_400)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Partial);

    // ANY group: first child success completes.
    let database2 = TestDatabase::new("aggregate-any");
    let authority2 = database2.open();
    register_task(&authority2);
    let any = root_group_spec(0x09, CompletionMode::Any, FailureMode::CollectAll, 8, 4);
    authority2.register_group(any).expect("any group");
    let attempt_x = attempt_spec(0x1a, initial_snapshot());
    let attempt_y = attempt_spec(0x1b, initial_snapshot());
    admit_attempt(&authority2, &attempt_x, group_id(0x09));
    admit_attempt(&authority2, &attempt_y, group_id(0x09));
    commit_no_effect(&authority2, &attempt_x, 0x51);
    let record = authority2
        .refresh_group_aggregate(group_id(0x09), 8_500)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Completed);
    // attempt_y stays untouched (still open).
    assert_eq!(
        authority2
            .inspect_attempt(task_id(), attempt_y.attempt_id)
            .expect("y")
            .state,
        AttemptState::Created
    );

    // An empty sealed ALL group completes vacuously (documented
    // placeholder semantics).
    let database3 = TestDatabase::new("aggregate-empty");
    let authority3 = database3.open();
    register_task(&authority3);
    let empty = root_group_spec(0x11, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority3.register_group(empty).expect("empty group");
    authority3.seal_group(group_id(0x11), 8_600).expect("seal");
    let record = authority3
        .refresh_group_aggregate(group_id(0x11), 8_700)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Completed);
}

/// Bullet: child failure honors the pre-committed `failure_mode` —
/// `FAIL_FAST` derives FAILED immediately and cancels remaining
/// non-terminal descendants in the same transaction; `COLLECT_ALL` waits
/// for every child to terminate; ISOLATE bulkheads the failure and
/// derives PARTIAL when at least one sibling succeeded.
#[test]
fn failure_modes_fail_fast_collect_all_isolate() {
    // FAIL_FAST.
    let database = TestDatabase::new("fail-fast");
    let authority = database.open();
    register_task(&authority);
    let group = root_group_spec(0x01, CompletionMode::All, FailureMode::FailFast, 8, 4);
    authority.register_group(group).expect("group");
    let failing = attempt_spec(0x0a, initial_snapshot());
    let pending = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority, &failing, group_id(0x01));
    admit_attempt(&authority, &pending, group_id(0x01));
    fail_attempt(&authority, &failing, 0x61);
    let record = authority
        .refresh_group_aggregate(group_id(0x01), 8_000)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Failed);
    assert_eq!(record.cancel_epoch, 1);
    // The remaining pre-permit member was cancelled by the FAIL_FAST
    // propagation, with a closure receipt and unchanged head.
    let pending_record = authority
        .inspect_attempt(task_id(), pending.attempt_id)
        .expect("pending");
    assert_eq!(pending_record.state, AttemptState::Cancelled);
    let receipt_id = pending_record.receipt_id.expect("closure receipt");
    let receipt = authority
        .inspect_receipt(task_id(), receipt_id)
        .expect("receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::CancelledBeforeEffect);

    // COLLECT_ALL: failure waits for all children to terminate.
    let database2 = TestDatabase::new("collect-all");
    let authority2 = database2.open();
    register_task(&authority2);
    let group2 = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority2.register_group(group2).expect("group");
    let failing2 = attempt_spec(0x0a, initial_snapshot());
    let pending2 = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority2, &failing2, group_id(0x01));
    admit_attempt(&authority2, &pending2, group_id(0x01));
    fail_attempt(&authority2, &failing2, 0x62);
    let record = authority2
        .refresh_group_aggregate(group_id(0x01), 8_100)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Open, "COLLECT_ALL waits");
    // The pending sibling is NOT cancelled by COLLECT_ALL.
    assert_eq!(
        authority2
            .inspect_attempt(task_id(), pending2.attempt_id)
            .expect("pending")
            .state,
        AttemptState::Created
    );
    // The failure advanced head + retry fence, so the stale sibling
    // terminally loses the permit CAS (Conflicted, Cancelled-class); a
    // fresh sibling admitted against the current head commits.
    let decision = authority2
        .request_commit_permit(permit_request(&pending2, 0x63, vec![]))
        .expect("pending competes");
    assert!(matches!(decision, PermitDecision::Conflicted { .. }));
    let head2 = authority2.inspect_task(task_id()).expect("head");
    let succeeding2 = attempt_spec(
        0x0c,
        snapshot_exact(
            0x14,
            head2.head_commit_seq,
            head2.head_effect_history_root,
            1,
        ),
    );
    admit_attempt(&authority2, &succeeding2, group_id(0x01));
    commit_no_effect_fence(&authority2, &succeeding2, 0x64, 1);
    let record = authority2
        .refresh_group_aggregate(group_id(0x01), 8_200)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Failed);

    // ISOLATE: one failure + one success → PARTIAL (bulkhead).
    let database3 = TestDatabase::new("isolate");
    let authority3 = database3.open();
    register_task(&authority3);
    let group3 = root_group_spec(0x01, CompletionMode::All, FailureMode::Isolate, 8, 4);
    authority3.register_group(group3).expect("group");
    let failing3 = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority3, &failing3, group_id(0x01));
    fail_attempt(&authority3, &failing3, 0x65);
    // The failure advanced head + retry fence to 1; a sibling admitted
    // against the current head commits.
    let head3 = authority3.inspect_task(task_id()).expect("head");
    let succeeding3 = attempt_spec(
        0x0b,
        snapshot_exact(
            0x14,
            head3.head_commit_seq,
            head3.head_effect_history_root,
            1,
        ),
    );
    admit_attempt(&authority3, &succeeding3, group_id(0x01));
    commit_no_effect_fence(&authority3, &succeeding3, 0x66, 1);
    let record = authority3
        .refresh_group_aggregate(group_id(0x01), 8_300)
        .expect("refresh");
    assert_eq!(record.state, GroupState::Partial);
}

/// Commit helper for heads with a non-zero retry fence (the legacy
/// finalize keeps the fence, so pass the current fence through).
fn commit_no_effect_fence(
    authority: &SqliteTaskAuthority,
    spec: &AttemptSpec,
    seed: u8,
    fence: u64,
) {
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(spec, seed, vec![]))
            .expect("permit"),
    );
    match authority
        .finalize_commit(nlos_task::FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: authority
                .inspect_task(spec.task_id)
                .expect("head")
                .head_effect_history_root,
            new_retry_fence_epoch: fence,
            finalized_at_ms: 7_000,
        })
        .expect("finalize")
    {
        FinalizeDecision::Committed(_) => {}
        FinalizeDecision::Replayed(_) => panic!("expected fresh commit"),
    }
}

/// Bullet: quarantine evidence anywhere in the subtree caps the parent
/// at PARTIAL — never COMPLETED (`[TASK-GROUP-002]` final clause);
/// quarantined members cannot be removed; group cancellation with
/// quarantine lands PARTIAL, not CANCELLED.
#[test]
fn quarantined_descendant_caps_parent_at_partial() {
    let database = TestDatabase::new("quarantine-cap");
    let authority = database.open();
    register_task(&authority);
    // ANY group: one quarantined member + one successful member.
    let group = root_group_spec(0x01, CompletionMode::Any, FailureMode::CollectAll, 8, 4);
    authority.register_group(group).expect("group");
    let succeeding = attempt_spec(0x0b, initial_snapshot());
    admit_attempt(&authority, &succeeding, group_id(0x01));
    // The sibling commits first: the quarantine tombstone blocks new
    // winner issuance task-wide (`[TASK-COMMIT-003]`), so the success
    // must be durable before the tombstone lands; the quarantined
    // member is admitted afterwards against the current head.
    commit_no_effect(&authority, &succeeding, 0x71);
    let head = authority.inspect_task(task_id()).expect("head");
    let quarantined = attempt_spec(
        0x0a,
        snapshot_exact(0x15, head.head_commit_seq, head.head_effect_history_root, 0),
    );
    admit_attempt(&authority, &quarantined, group_id(0x01));
    quarantine_attempt(&authority, &quarantined, 0x72);
    // The quarantined member cannot be removed (evidence laundering
    // refused) — checked while the group is still OPEN.
    assert!(matches!(
        authority.remove_member(RemoveMemberRequest {
            group_id: group_id(0x01),
            member: GroupMemberRef {
                member_type: GroupMemberType::TaskAttempt,
                member_id: quarantined.attempt_id.into_bytes(),
                member_generation: quarantined.attempt_generation,
            },
            removed_at_ms: 8_100,
        }),
        Err(TaskStoreError::GroupQuarantinedChild)
    ));
    let record = authority
        .refresh_group_aggregate(group_id(0x01), 8_000)
        .expect("refresh");
    assert_eq!(
        record.state,
        GroupState::Partial,
        "quarantine caps ANY success at PARTIAL, never COMPLETED"
    );

    // Group cancellation over a quarantined subtree lands PARTIAL.
    let database2 = TestDatabase::new("quarantine-cancel");
    let authority2 = database2.open();
    register_task(&authority2);
    let root2 = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority2.register_group(root2).expect("root");
    let child2 = child_group_spec(0x02, group_id(0x01), 8, 3);
    authority2.register_group(child2).expect("child");
    let quarantined2 = attempt_spec(0x0a, initial_snapshot());
    admit_attempt(&authority2, &quarantined2, group_id(0x02));
    quarantine_attempt(&authority2, &quarantined2, 0x73);
    let decision = authority2
        .cancel_group(GroupCancelRequest {
            group_id: group_id(0x01),
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xc7)),
            requested_at_ms: 9_000,
        })
        .expect("cancel");
    assert!(matches!(decision, GroupCancelDecision::Applied { .. }));
    assert_eq!(
        authority2
            .inspect_group(group_id(0x02))
            .expect("child")
            .state,
        GroupState::Partial,
        "child group with quarantine lands PARTIAL, not CANCELLED"
    );
    assert_eq!(
        authority2
            .inspect_group(group_id(0x01))
            .expect("root")
            .state,
        GroupState::Partial,
        "parent with quarantined descendant lands PARTIAL"
    );
}

/// Frozen golden copy of the B-TASK-003 v3 schema: v1 + v2 + v3 DDL,
/// intentionally duplicated so the fixture cannot drift with the live
/// source.
const GOLDEN_V3_DDL: &str = concat!(
    include_str!("golden_v1_ddl.sql"),
    "\n",
    include_str!("golden_v2_ddl.sql"),
    "\n",
    include_str!("golden_v3_ddl.sql")
);

fn blob8(value: u64) -> Vec<u8> {
    value.to_be_bytes().to_vec()
}

/// Seeds a complete v3 database: one task, one snapshot, one
/// `CommitPermitted` attempt, one outstanding permit (ID `0x77..`) with
/// a one-slot declared effect set in `Planned` state (the same shape as
/// the golden v2 fixture, which is a valid subset of v3).
fn seed_golden_v3(database: &TestDatabase) {
    seed_golden_v3_core(database);
    seed_golden_v3_effects(database);
}

fn seed_golden_v3_core(database: &TestDatabase) {
    let connection = rusqlite::Connection::open(&database.path).expect("create raw v3 database");
    connection
        .execute_batch(GOLDEN_V3_DDL)
        .expect("create golden v3 schema");
    connection
        .execute(
            "INSERT INTO tasks (
                task_id, task_generation, head_commit_seq, head_effect_history_root,
                retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
                task_state, revision, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 1, 1000, 3000)",
            rusqlite::params![
                bytes(0x01).as_slice(),
                blob8(1),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
                blob8(2),
                blob8(0),
                blob8(1),
            ],
        )
        .expect("seed task");
    connection
        .execute(
            "INSERT INTO task_snapshots (
                task_id, snapshot_id, snapshot_digest, expected_head_commit_seq,
                effect_history_root, retry_fence_epoch, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 2000)",
            rusqlite::params![
                bytes(0x01).as_slice(),
                bytes(0x10).as_slice(),
                [0x20u8; 32].as_slice(),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
            ],
        )
        .expect("seed snapshot");
    connection
        .execute(
            "INSERT INTO task_attempts (
                attempt_id, task_id, attempt_generation, snapshot_id,
                cancellation_scope_id, cancellation_generation, idempotency_key,
                attempt_state, receipt_id, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 10, NULL, 2000, 3000)",
            rusqlite::params![
                bytes(0x0a).as_slice(),
                bytes(0x01).as_slice(),
                blob8(1),
                bytes(0x10).as_slice(),
                bytes(0xca).as_slice(),
                blob8(1),
                bytes(0xaa).as_slice(),
            ],
        )
        .expect("seed attempt");
    connection
        .execute(
            "INSERT INTO commit_permits (
                permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
                expected_head_commit_seq, expected_effect_history_root,
                expected_retry_fence_epoch, write_set_root, permit_epoch,
                control_epoch, cancel_epoch, valid_until_ms, permit_state,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 9999, 0, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                bytes(0x01).as_slice(),
                bytes(0xb1).as_slice(),
                bytes(0x0a).as_slice(),
                blob8(1),
                blob8(0),
                empty_effect_history_root().as_slice(),
                blob8(0),
                [0x01u8; 32].as_slice(),
                blob8(1),
                blob8(2),
                blob8(0),
            ],
        )
        .expect("seed permit");
}

fn seed_golden_v3_effects(database: &TestDatabase) {
    let connection = rusqlite::Connection::open(&database.path).expect("raw v3 connection");
    connection
        .execute(
            "INSERT INTO effect_slots (
                permit_id, effect_seq, task_id, effect_slot_id, logical_effect_id,
                idempotency_identity_digest, required, required_condition_digest,
                success_criteria_digest, action_proposal_digest, slot_state,
                state_seq, effect_permit_id, effect_receipt_id,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, NULL, ?7, ?8, 0, 0, NULL, NULL, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                blob8(0),
                bytes(0x01).as_slice(),
                bytes(0x78).as_slice(),
                descriptor(0).logical_effect_id().as_slice(),
                descriptor(0).idempotency_identity_digest().as_slice(),
                [0x66u8; 32].as_slice(),
                [0x77u8; 32].as_slice(),
            ],
        )
        .expect("seed slot");
    connection
        .execute(
            "INSERT INTO permit_effect_sets (
                permit_id, task_id, effect_set_root, effect_slot_state_root,
                required_effect_count, satisfied_required_effect_count,
                terminal_effect_count, issued_effect_root, dispatched_effect_root,
                closed_effect_root, outstanding_effect_root, revision,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, 1, 0, 0, ?5, ?5, ?5, ?5, 0, 3000, 3000)",
            rusqlite::params![
                bytes(0x77).as_slice(),
                bytes(0x01).as_slice(),
                [0x02u8; 32].as_slice(),
                [0x03u8; 32].as_slice(),
                [0x04u8; 32].as_slice(),
            ],
        )
        .expect("seed effect set");
}

/// Bullet: schema migration is lossless — every v3 row survives, legacy
/// permits remain explicitly participant-unbound, the new group plane is
/// usable, and old/new immutability triggers survive.
#[test]
#[allow(clippy::too_many_lines)] // One migration test audits legacy and current planes together.
fn golden_v3_database_migrates_losslessly_to_v4() {
    let database = TestDatabase::new("golden-v3");
    seed_golden_v3(&database);
    let seeded_permit_id = CommitPermitId::from_bytes(bytes(0x77));

    let authority = database.open();
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        let version: i64 = connection
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version");
        assert_eq!(version, 24, "migration stamps the current schema version");
    }

    // All v3 data intact.
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.active_permit, Some(seeded_permit_id));
    let slot = authority
        .inspect_effect_slot(seeded_permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, nlos_task::SlotState::Planned);
    assert!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .is_empty()
    );

    // Migration cannot invent a participant registry binding for the
    // seeded permit, so new effect and terminal mutations fail closed.
    let spec_a = attempt_spec(0x0a, initial_snapshot());
    let permit = authority
        .inspect_permit(task_id(), seeded_permit_id)
        .expect("permit");
    assert_eq!(permit.participant_registry_binding, None);
    assert!(matches!(
        authority.request_effect_permit(EffectPermitRequest {
            task_id: spec_a.task_id,
            attempt_id: spec_a.attempt_id,
            attempt_generation: spec_a.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes(bytes(0xe1)),
            valid_until_ms: 9_999,
            requested_at_ms: 4_000,
        }),
        Err(TaskStoreError::ParticipantRegistryBindingMissing)
    ));
    assert!(matches!(
        authority.finalize_commit_v3(FinalizeRequestV3 {
            base: nlos_task::FinalizeRequest {
                task_id: spec_a.task_id,
                attempt_id: spec_a.attempt_id,
                attempt_generation: spec_a.attempt_generation,
                permit_id: seeded_permit_id,
                new_effect_history_root: [0u8; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: 7_000,
            },
            required_satisfaction: vec![nlos_task::RequiredSatisfaction {
                effect_seq: 0,
                proof: nlos_task::RequiredSatisfactionProof::EffectClosedSuccess {
                    success_assertion_digest: [0x5a; 32],
                },
            }],
            fenced_participant_digest: [0xf1; 32],
        }),
        Err(TaskStoreError::ParticipantRegistryBindingMissing)
    ));

    // The v4 plane starts empty and is fully usable: root group,
    // membership admission, receipts, cancellation.
    let root = root_group_spec(0x01, CompletionMode::All, FailureMode::CollectAll, 8, 4);
    authority
        .register_group(root)
        .expect("root group on migrated db");
    let head_after = authority.inspect_task(task_id()).expect("head");
    let attempt_b = attempt_spec(
        0x0b,
        snapshot_exact(
            0x13,
            head_after.head_commit_seq,
            head_after.head_effect_history_root,
            0,
        ),
    );
    let admission = admit_attempt(&authority, &attempt_b, group_id(0x01));
    let record = authority.inspect_group(group_id(0x01)).expect("group");
    assert_eq!(record.membership_generation, 1);
    let receipts = authority
        .list_group_receipts(group_id(0x01))
        .expect("receipts");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].receipt_id, admission.admission_receipt_id);

    // The v3 immutability triggers still enforce, and the v4 ones do
    // too (a receipt row exists now, so the UPDATE reaches the trigger).
    let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
    let effect_trigger: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type='trigger' AND name='effect_receipt_is_immutable'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(effect_trigger, 1);
    assert!(
        connection
            .execute(
                "UPDATE task_group_admission_receipts SET created_at_ms = created_at_ms",
                []
            )
            .is_err(),
        "v4 group receipt trigger enforces"
    );
}

/// Bullet: v3 → v4 migration is transactional and fail-closed — a v3
/// database on which the v4 step cannot complete stays a complete v3
/// database, never a half-migrated one.
#[test]
fn failed_v4_migration_rolls_back_to_complete_v3() {
    let database = TestDatabase::new("migration-v4-failure");
    seed_golden_v3(&database);
    {
        let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
        connection
            .execute_batch("CREATE TABLE task_groups (bogus INTEGER) STRICT;")
            .expect("plant conflicting table");
    }

    assert!(
        SqliteTaskAuthority::open(&database.path).is_err(),
        "migration conflict must fail closed"
    );

    let connection = rusqlite::Connection::open(&database.path).expect("raw connection");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(version, 3, "rolled-back migration keeps the v3 stamp");
    let permits: i64 = connection
        .query_row("SELECT COUNT(*) FROM commit_permits", [], |row| row.get(0))
        .expect("v3 permit data intact");
    assert_eq!(permits, 1);
    let slots: i64 = connection
        .query_row("SELECT COUNT(*) FROM effect_slots", [], |row| row.get(0))
        .expect("v3 slot data intact");
    assert_eq!(slots, 1);
    let v4_tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN (
                'task_group_members', 'task_group_admission_receipts',
                'task_group_cancels', 'task_attempt_group_bindings')",
            [],
            |row| row.get(0),
        )
        .expect("inspect schema");
    assert_eq!(v4_tables, 0, "no partial v4 table may survive the rollback");
}
