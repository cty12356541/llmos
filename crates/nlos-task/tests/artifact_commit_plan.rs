//! B-TASK-006A/B: durable Artifact publication plan and nested receipts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    ArtifactCommitPlanDecision, ArtifactCommitPlanState, ArtifactFinalizeDecision,
    ArtifactPublicationAuthorizationDecision, ArtifactPublicationExpectation, AttemptSpec,
    FinalizeArtifactCommitRequest, LogicalEffectDescriptor, NestedArtifactPublicationReceipt,
    PermitDecision, PermitRequest, PlanArtifactCommitRequest, PlannedEffect,
    RecordArtifactPublicationsRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    TaskStoreError, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-artifact-plan-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open authority")
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

fn expectation(seed: u8, revision: u64) -> ArtifactPublicationExpectation {
    ArtifactPublicationExpectation {
        staging_id: [0x20 + seed; 16],
        artifact_id: ArtifactId::from_bytes([0x40 + seed; 16]),
        target_revision: revision,
        digest: [0x60 + seed; 32],
        size_bytes: 100 + u64::from(seed),
    }
}

fn register_and_issue(
    authority: &SqliteTaskAuthority,
    expectations: &[ArtifactPublicationExpectation],
) -> (AttemptSpec, nlos_task::PermitRecord) {
    register_and_issue_with_effects(authority, expectations, Vec::new())
}

fn register_and_issue_with_effects(
    authority: &SqliteTaskAuthority,
    expectations: &[ArtifactPublicationExpectation],
    planned_effects: Vec<PlannedEffect>,
) -> (AttemptSpec, nlos_task::PermitRecord) {
    let task_id = TaskId::from_bytes([0x01; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x02; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x03; 16]),
            snapshot_digest: [0x04; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x05; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
        registered_at_ms: 2_000,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    let write_set_root = artifact_publication_plan_root(expectations).expect("plan root");
    let decision = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root,
            planned_effects,
            idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .expect("issue permit");
    let PermitDecision::Issued(permit) = decision else {
        panic!("expected issued permit, got {decision:?}");
    };
    (attempt, *permit)
}

fn plan_request(
    attempt: AttemptSpec,
    permit: &nlos_task::PermitRecord,
    expectations: Vec<ArtifactPublicationExpectation>,
) -> PlanArtifactCommitRequest {
    PlanArtifactCommitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        permit_id: permit.permit_id,
        expectations,
        idempotency_key: IdempotencyKey::from_bytes([0x08; 16]),
        planned_at_ms: 4_000,
    }
}

fn publication(
    expectation: ArtifactPublicationExpectation,
    plan: &nlos_task::ArtifactCommitPlanRecord,
    seed: u8,
) -> NestedArtifactPublicationReceipt {
    let prior_head_revision = expectation.target_revision - 1;
    NestedArtifactPublicationReceipt {
        receipt_id: ReceiptId::from_bytes([0xa0 + seed; 16]),
        staging_id: expectation.staging_id,
        artifact_id: expectation.artifact_id,
        revision: expectation.target_revision,
        digest: expectation.digest,
        size_bytes: expectation.size_bytes,
        task_id: plan.task_id,
        permit_id: plan.permit_id,
        write_set_root: plan.write_set_root,
        prior_head_revision,
        prior_head_digest: (prior_head_revision != 0).then_some([0xb0 + seed; 32]),
        new_head_revision: expectation.target_revision,
        new_head_digest: expectation.digest,
        created_at_ms: 5_000 + i64::from(seed),
    }
}

fn authorize(authority: &SqliteTaskAuthority, plan: &nlos_task::ArtifactCommitPlanRecord) {
    assert!(matches!(
        authority
            .authorize_artifact_publication(plan.plan_id, 4_500)
            .expect("authorize publication"),
        ArtifactPublicationAuthorizationDecision::Authorized(_)
    ));
}

#[test]
fn authorization_is_durable_replayable_and_required_before_receipts() {
    let database = TestDatabase::new("authorization");
    let expectations = vec![expectation(1, 1)];
    let (attempt, permit, plan) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let plan = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
            .unwrap()
            .record()
            .clone();
        let receipt = publication(expectation(1, 1), &plan, 1);
        assert!(matches!(
            authority.record_artifact_publications(RecordArtifactPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![receipt],
                observed_at_ms: 4_250,
            }),
            Err(TaskStoreError::ArtifactPublicationConflict { .. })
        ));
        let decision = authority
            .authorize_artifact_publication(plan.plan_id, 4_500)
            .unwrap();
        assert!(matches!(
            decision,
            ArtifactPublicationAuthorizationDecision::Authorized(_)
        ));
        assert_eq!(decision.record().state, ArtifactCommitPlanState::Publishing);
        let progress = authority
            .inspect_artifact_commit_progress(plan.plan_id)
            .unwrap();
        assert!(progress.publications.is_empty());
        (attempt, permit, plan)
    };

    let reopened = database.open();
    let replay = reopened
        .authorize_artifact_publication(plan.plan_id, 9_000)
        .unwrap();
    assert!(matches!(
        replay,
        ArtifactPublicationAuthorizationDecision::Replayed(_)
    ));
    assert_eq!(replay.record().updated_at_ms, 4_500);
    assert_eq!(
        reopened
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
    assert_eq!(
        reopened
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
}

#[test]
fn authorization_rejects_permits_with_effect_slots() {
    let database = TestDatabase::new("authorization-effect-rejection");
    let authority = database.open();
    let expectations = vec![expectation(1, 1)];
    let effect = PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: TaskId::from_bytes([0x01; 16]),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x31; 32],
            stable_action_slot: 1,
            target_authority_object_id: [0x32; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x33; 32],
        action_proposal_digest: [0x34; 32],
    };
    let (attempt, permit) =
        register_and_issue_with_effects(&authority, &expectations, vec![effect]);
    let plan = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations))
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        authority.authorize_artifact_publication(plan.plan_id, 4_500),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    assert_eq!(
        authority
            .inspect_artifact_commit_plan(plan.plan_id)
            .unwrap()
            .state,
        ArtifactCommitPlanState::Planned
    );
}

#[test]
fn ready_plan_finalizes_atomically_and_replays_complete_receipt_after_restart() {
    let database = TestDatabase::new("artifact-finalize");
    let expectations = vec![expectation(1, 1), expectation(2, 1)];
    let (attempt, permit, plan, expected_publications, committed) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let plan = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
            .unwrap()
            .record()
            .clone();
        authorize(&authority, &plan);
        assert!(matches!(
            authority.finalize_artifact_commit(FinalizeArtifactCommitRequest {
                plan_id: plan.plan_id,
                finalized_at_ms: 5_500,
            }),
            Err(TaskStoreError::ArtifactCommitPlanNotReady {
                state: ArtifactCommitPlanState::Publishing
            })
        ));
        let expected_publications = vec![
            publication(expectation(1, 1), &plan, 1),
            publication(expectation(2, 1), &plan, 2),
        ];
        authority
            .record_artifact_publications(RecordArtifactPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: expected_publications.clone(),
                observed_at_ms: 6_000,
            })
            .unwrap();
        let decision = authority
            .finalize_artifact_commit(FinalizeArtifactCommitRequest {
                plan_id: plan.plan_id,
                finalized_at_ms: 7_000,
            })
            .unwrap();
        assert!(matches!(decision, ArtifactFinalizeDecision::Committed(_)));
        let committed = decision.receipt().clone();
        assert_eq!(committed.artifact_publications, expected_publications);
        assert_eq!(committed.task_receipt.new_head_commit_seq, 1);
        assert_eq!(
            committed.task_receipt.new_effect_history_root,
            empty_effect_history_root()
        );
        let finalized = authority
            .inspect_artifact_commit_plan(plan.plan_id)
            .unwrap();
        assert_eq!(finalized.state, ArtifactCommitPlanState::Finalized);
        assert_eq!(
            finalized.task_receipt_id,
            Some(committed.task_receipt.receipt_id)
        );
        assert_eq!(
            authority
                .inspect_task(attempt.task_id)
                .unwrap()
                .head_commit_seq,
            1
        );
        assert_eq!(
            authority
                .inspect_permit(attempt.task_id, permit.permit_id)
                .unwrap()
                .state,
            nlos_task::PermitState::Closed
        );
        (attempt, permit, plan, expected_publications, committed)
    };

    let reopened = database.open();
    let replay = reopened
        .finalize_artifact_commit(FinalizeArtifactCommitRequest {
            plan_id: plan.plan_id,
            finalized_at_ms: 9_000,
        })
        .unwrap();
    assert!(matches!(replay, ArtifactFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &committed);
    assert_eq!(
        replay.receipt().artifact_publications,
        expected_publications
    );
    assert_eq!(
        reopened
            .inspect_receipt(attempt.task_id, committed.task_receipt.receipt_id)
            .unwrap(),
        committed.task_receipt
    );
    assert_eq!(
        reopened
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Closed
    );
}

#[test]
fn finalize_storage_failure_rolls_back_every_terminal_fact() {
    let database = TestDatabase::new("artifact-finalize-rollback");
    let expectations = vec![expectation(1, 1)];
    let (attempt, permit, plan) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let plan = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations))
            .unwrap()
            .record()
            .clone();
        authorize(&authority, &plan);
        authority
            .record_artifact_publications(RecordArtifactPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![publication(expectation(1, 1), &plan, 1)],
                observed_at_ms: 6_000,
            })
            .unwrap();
        (attempt, permit, plan)
    };
    let raw = Connection::open(&database.path).unwrap();
    raw.execute_batch(
        "CREATE TRIGGER fail_artifact_finalize
         BEFORE UPDATE ON task_artifact_commit_plans
         WHEN NEW.plan_state = 3
         BEGIN SELECT RAISE(ABORT, 'injected finalize failure'); END;",
    )
    .unwrap();
    drop(raw);

    let authority = database.open();
    assert!(matches!(
        authority.finalize_artifact_commit(FinalizeArtifactCommitRequest {
            plan_id: plan.plan_id,
            finalized_at_ms: 7_000,
        }),
        Err(TaskStoreError::Sqlite(_))
    ));
    assert_eq!(
        authority
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
    assert_eq!(
        authority
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
    assert_eq!(
        authority
            .inspect_artifact_commit_plan(plan.plan_id)
            .unwrap()
            .state,
        ArtifactCommitPlanState::Ready
    );
    drop(authority);

    let raw = Connection::open(&database.path).unwrap();
    raw.execute_batch("DROP TRIGGER fail_artifact_finalize;")
        .unwrap();
    drop(raw);
    assert!(matches!(
        database
            .open()
            .finalize_artifact_commit(FinalizeArtifactCommitRequest {
                plan_id: plan.plan_id,
                finalized_at_ms: 8_000,
            })
            .unwrap(),
        ArtifactFinalizeDecision::Committed(_)
    ));
}

#[test]
fn plan_root_is_order_independent_and_rejects_ambiguous_sets() {
    let left = expectation(1, 1);
    let right = expectation(2, 3);
    assert_eq!(
        artifact_publication_plan_root(&[left, right]).unwrap(),
        artifact_publication_plan_root(&[right, left]).unwrap()
    );
    assert!(matches!(
        artifact_publication_plan_root(&[]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    let mut duplicate_staging = right;
    duplicate_staging.staging_id = left.staging_id;
    assert!(matches!(
        artifact_publication_plan_root(&[left, duplicate_staging]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    let mut duplicate_revision = right;
    duplicate_revision.artifact_id = left.artifact_id;
    duplicate_revision.target_revision = left.target_revision;
    assert!(matches!(
        artifact_publication_plan_root(&[left, duplicate_revision]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
}

#[test]
fn plan_binds_issued_permit_without_advancing_task_head() {
    let database = TestDatabase::new("plan");
    let authority = database.open();
    let expectations = vec![expectation(2, 1), expectation(1, 4)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);

    let decision = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
        .expect("plan");
    assert!(matches!(decision, ArtifactCommitPlanDecision::Planned(_)));
    let record = decision.record();
    assert_eq!(record.state, ArtifactCommitPlanState::Planned);
    assert_eq!(record.write_set_root, permit.write_set_root);
    assert_eq!(
        record.expectations,
        vec![expectation(1, 4), expectation(2, 1)]
    );
    assert_eq!(
        authority
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
    assert_eq!(
        authority
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
}

#[test]
fn exact_plan_replays_across_restart_and_conflicting_key_fails_closed() {
    let database = TestDatabase::new("replay");
    let expectations = vec![expectation(1, 1), expectation(2, 1)];
    let (attempt, permit, first) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let first = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
            .unwrap()
            .record()
            .clone();
        (attempt, permit, first)
    };
    let reopened = database.open();
    let replay = reopened
        .plan_artifact_commit(plan_request(
            attempt,
            &permit,
            vec![expectation(2, 1), expectation(1, 1)],
        ))
        .expect("replay");
    assert!(matches!(replay, ArtifactCommitPlanDecision::Replayed(_)));
    assert_eq!(replay.record(), &first);
    assert_eq!(
        reopened
            .inspect_artifact_commit_plan(first.plan_id)
            .unwrap(),
        first
    );

    let mut changed = expectation(2, 1);
    changed.digest = [0xee; 32];
    assert!(matches!(
        reopened.plan_artifact_commit(plan_request(
            attempt,
            &permit,
            vec![expectation(1, 1), changed],
        )),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

#[test]
fn plan_root_must_equal_permit_write_set_root() {
    let database = TestDatabase::new("root-mismatch");
    let authority = database.open();
    let original = vec![expectation(1, 1)];
    let (attempt, permit) = register_and_issue(&authority, &original);
    assert!(matches!(
        authority.plan_artifact_commit(plan_request(attempt, &permit, vec![expectation(2, 1)],)),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    assert_eq!(
        authority
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
}

#[test]
fn plan_identity_and_expectations_are_ddl_immutable() {
    let database = TestDatabase::new("immutable");
    let authority = database.open();
    let expectations = vec![expectation(1, 1)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);
    let plan = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations))
        .unwrap()
        .record()
        .clone();
    drop(authority);

    let raw = Connection::open(&database.path).expect("open raw");
    assert!(
        raw.execute(
            "UPDATE task_artifact_commit_plans SET write_set_root = zeroblob(32)
             WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_artifact_publication_expectations WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_artifact_commit_plans WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn v5_database_migrates_to_v6_without_changing_existing_task() {
    let database = TestDatabase::new("migration");
    {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: TaskId::from_bytes([0x33; 16]),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
    }
    let raw = Connection::open(&database.path).expect("open raw");
    raw.execute_batch(
        "DROP TABLE task_artifact_publication_receipts;
         DROP TABLE task_artifact_publication_expectations;
         DROP TABLE task_artifact_commit_plans;
         PRAGMA user_version = 5;",
    )
    .expect("restore structural v5");
    drop(raw);

    let migrated = database.open();
    assert_eq!(
        migrated
            .inspect_task(TaskId::from_bytes([0x33; 16]))
            .unwrap()
            .head_commit_seq,
        0
    );
    drop(migrated);
    let raw = Connection::open(&database.path).expect("reopen raw");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 7);
}

#[test]
fn partial_publications_survive_restart_and_complete_to_ready() {
    let database = TestDatabase::new("publication-progress");
    let expectations = vec![expectation(1, 1), expectation(2, 1)];
    let (attempt, permit, plan, first_receipt) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let plan = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
            .unwrap()
            .record()
            .clone();
        authorize(&authority, &plan);
        let first_receipt = publication(expectation(1, 1), &plan, 1);
        let progress = authority
            .record_artifact_publications(RecordArtifactPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![first_receipt],
                observed_at_ms: 6_000,
            })
            .expect("record partial");
        assert_eq!(progress.plan.state, ArtifactCommitPlanState::Publishing);
        assert_eq!(progress.publications, vec![first_receipt]);
        assert_eq!(
            authority
                .inspect_task(attempt.task_id)
                .unwrap()
                .head_commit_seq,
            0
        );
        (attempt, permit, plan, first_receipt)
    };

    let reopened = database.open();
    let partial = reopened
        .inspect_artifact_commit_progress(plan.plan_id)
        .expect("inspect partial after restart");
    assert_eq!(partial.plan.state, ArtifactCommitPlanState::Publishing);
    assert_eq!(partial.publications, vec![first_receipt]);

    let second_receipt = publication(expectation(2, 1), &plan, 2);
    let ready = reopened
        .record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![second_receipt],
            observed_at_ms: 7_000,
        })
        .expect("complete receipts");
    assert_eq!(ready.plan.state, ArtifactCommitPlanState::Ready);
    assert_eq!(ready.publications.len(), 2);
    assert_eq!(
        reopened
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
    assert_eq!(
        reopened
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );

    let replay = reopened
        .record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![first_receipt, second_receipt],
            observed_at_ms: 9_000,
        })
        .expect("exact replay");
    assert_eq!(replay, ready, "exact replay does not rewrite timestamps");
}

#[test]
fn conflicting_receipt_batch_rolls_back_without_partial_consumption() {
    let database = TestDatabase::new("publication-conflict");
    let authority = database.open();
    let expectations = vec![expectation(1, 1), expectation(2, 1)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);
    let plan = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations))
        .unwrap()
        .record()
        .clone();
    authorize(&authority, &plan);
    let valid = publication(expectation(1, 1), &plan, 1);
    let mut invalid = publication(expectation(2, 1), &plan, 2);
    invalid.write_set_root = [0xff; 32];

    assert!(matches!(
        authority.record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![valid, invalid],
            observed_at_ms: 6_000,
        }),
        Err(TaskStoreError::ArtifactPublicationConflict { .. })
    ));
    let unchanged = authority
        .inspect_artifact_commit_progress(plan.plan_id)
        .unwrap();
    assert_eq!(unchanged.plan.state, ArtifactCommitPlanState::Publishing);
    assert!(unchanged.publications.is_empty());
}

#[test]
fn nested_publication_receipts_are_ddl_immutable() {
    let database = TestDatabase::new("publication-immutable");
    let authority = database.open();
    let expectations = vec![expectation(1, 1)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);
    let plan = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations))
        .unwrap()
        .record()
        .clone();
    authorize(&authority, &plan);
    let receipt = publication(expectation(1, 1), &plan, 1);
    authority
        .record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![receipt],
            observed_at_ms: 6_000,
        })
        .unwrap();
    drop(authority);

    let raw = Connection::open(&database.path).unwrap();
    assert!(
        raw.execute(
            "UPDATE task_artifact_publication_receipts SET created_at_ms = created_at_ms + 1
             WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_artifact_publication_receipts WHERE receipt_id = ?1",
            [receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn v6_plan_migrates_to_v7_and_remains_queryable() {
    let database = TestDatabase::new("migration-v7");
    let (plan_id, expected) = {
        let authority = database.open();
        let expectations = vec![expectation(1, 1)];
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let plan = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations))
            .unwrap()
            .record()
            .clone();
        (plan.plan_id, plan)
    };
    let raw = Connection::open(&database.path).unwrap();
    raw.execute_batch(
        "DROP TABLE task_artifact_publication_receipts;
         PRAGMA user_version = 6;",
    )
    .expect("restore structural v6");
    drop(raw);

    let migrated = database.open();
    assert_eq!(
        migrated.inspect_artifact_commit_plan(plan_id).unwrap(),
        expected
    );
    assert!(
        migrated
            .inspect_artifact_commit_progress(plan_id)
            .unwrap()
            .publications
            .is_empty()
    );
}
