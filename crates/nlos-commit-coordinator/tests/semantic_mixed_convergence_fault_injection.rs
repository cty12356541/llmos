//! Bounded fault-prefix coverage for mixed Effect + Semantic convergence.
//!
//! The existing Semantic coordinator fault matrix uses a Semantic-only
//! write-set, while the mixed restart scan covers only the no-fault path. This
//! integration binary keeps the production code unchanged and exercises the
//! three mixed durable boundaries through the public `converge_pending`
//! restart entry point: owner publication, Task-side receipt consumption, and
//! terminal mixed finalization.

mod mixed {
    include!("semantic_convergence.rs");

    fn prepare_mixed(
        fixture: &Fixture,
    ) -> (
        SqliteTaskAuthority,
        SemanticAuthority,
        SemanticCommitPlanId,
        TaskId,
        TaskAttemptId,
        CommitPermitId,
        u64,
    ) {
        let prepared = prepare(fixture, true);
        prepared
            .0
            .prepare_semantic_finalize(nlos_task::PrepareSemanticFinalizeRequest {
                plan_id: prepared.2,
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
                prepared_at_ms: 6,
            })
            .expect("persist mixed finalize envelope");
        prepared
    }

    fn close_effect(
        task: &SqliteTaskAuthority,
        task_id: TaskId,
        attempt_id: TaskAttemptId,
        permit_id: CommitPermitId,
        permit_epoch: u64,
    ) {
        let issued = match task
            .request_effect_permit(EffectPermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id,
                permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes([0xf4; 16]),
                valid_until_ms: 1_000,
                requested_at_ms: 9,
            })
            .expect("issue mixed Effect permit")
        {
            EffectPermitDecision::Issued(issued) | EffectPermitDecision::Replayed(issued) => {
                *issued
            }
        };
        task.record_no_effect(NoEffectRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id,
            permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: Some(issued.one_shot_dispatch_token),
            recorded_at_ms: 10,
        })
        .expect("close mixed Effect slot");
    }

    fn publication_count(root: &Path) -> usize {
        let connection = Connection::open(root.join("semantic-authority.db"))
            .expect("open Semantic inspection connection");
        usize::try_from(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM semantic_publication_receipts",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count Semantic publication receipts"),
        )
        .expect("publication count fits usize")
    }

    fn assert_mixed_finalized(
        task: &SqliteTaskAuthority,
        semantic_root: &Path,
        plan_id: SemanticCommitPlanId,
    ) {
        let progress = task
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect mixed Semantic progress");
        assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
        assert_eq!(progress.publications.len(), 1);
        assert_eq!(
            task.inspect_task(progress.plan.task_id)
                .expect("inspect mixed Task")
                .head_commit_seq,
            1
        );
        assert!(progress.plan.task_receipt_id.is_some());
        assert!(
            task.inspect_semantic_finalize_envelope(plan_id)
                .expect("inspect mixed finalize envelope")
                .is_some()
        );
        assert!(
            task.list_incomplete_semantic_commit_plans(1)
                .expect("scan incomplete mixed plans")
                .is_empty()
        );
        assert_eq!(publication_count(semantic_root), 1);
    }

    fn recover_pending(fixture: &Fixture, plan_id: SemanticCommitPlanId, now_ms: i64) {
        let task = SqliteTaskAuthority::open(&fixture.task_path)
            .expect("reopen Task authority for mixed pending scan");
        let semantic = SemanticAuthority::open(&fixture.semantic_root)
            .expect("reopen Semantic authority for mixed pending scan");
        let receipts = SemanticCommitCoordinator::new(&task, &semantic)
            .converge_pending(1, now_ms)
            .expect("converge mixed pending plan after restart");
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].semantic_publications.len(), 1);
        assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);
        assert_mixed_finalized(&task, &fixture.semantic_root, plan_id);
    }

    #[test]
    fn mixed_owner_failure_restarts_through_pending_scan() {
        let _serialization = fault_lock();
        nlos_store_fault::register(VFS_NAME).expect("register mixed coordinator fault VFS");
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, task_id, attempt_id, permit_id, permit_epoch) =
            prepare_mixed(&fixture);
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
                .expect("authorize mixed Semantic plan"),
            ConvergeSemanticStep::Authorized
        ));
        drop(semantic);

        let faulted = SemanticAuthority::open_with_vfs(&fixture.semantic_root, Some(VFS_NAME))
            .expect("reopen Semantic authority through fault VFS");
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let result = SemanticCommitCoordinator::new(&task, &faulted)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 });
        assert!(matches!(
            result,
            Err(nlos_commit_coordinator::CoordinatorError::Semantic(_))
        ));
        assert!(nlos_store_fault::writes_observed() > 0);
        drop(faulted);
        nlos_store_fault::disarm();
        assert_eq!(publication_count(&fixture.semantic_root), 0);
        close_effect(&task, task_id, attempt_id, permit_id, permit_epoch);
        drop(task);

        recover_pending(&fixture, plan_id, 9);
    }

    #[test]
    fn mixed_task_consumer_failure_restarts_through_pending_scan() {
        let _serialization = fault_lock();
        nlos_store_fault::register(VFS_NAME).expect("register mixed coordinator fault VFS");
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, task_id, attempt_id, permit_id, permit_epoch) =
            prepare_mixed(&fixture);
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
                .expect("authorize mixed Semantic plan"),
            ConvergeSemanticStep::Authorized
        ));
        close_effect(&task, task_id, attempt_id, permit_id, permit_epoch);
        drop(task);

        let faulted = SqliteTaskAuthority::open_with_vfs(&fixture.task_path, Some(VFS_NAME))
            .expect("reopen Task authority through fault VFS");
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::IoErr,
        });
        let result = SemanticCommitCoordinator::new(&faulted, &semantic)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 });
        assert!(matches!(
            result,
            Err(nlos_commit_coordinator::CoordinatorError::Task(_))
        ));
        assert!(nlos_store_fault::writes_observed() > 0);
        drop(faulted);
        drop(semantic);
        nlos_store_fault::disarm();
        assert_eq!(publication_count(&fixture.semantic_root), 1);

        recover_pending(&fixture, plan_id, 9);
    }

    #[test]
    fn mixed_finalize_failure_restarts_through_pending_scan() {
        let _serialization = fault_lock();
        nlos_store_fault::register(VFS_NAME).expect("register mixed coordinator fault VFS");
        nlos_store_fault::disarm();
        let _fault_guard = FaultDisarmGuard;
        let fixture = Fixture::new();
        let (task, semantic, plan_id, task_id, attempt_id, permit_id, permit_epoch) =
            prepare_mixed(&fixture);
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
                .expect("authorize mixed Semantic plan"),
            ConvergeSemanticStep::Authorized
        ));
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 })
                .expect("publish mixed Semantic owner receipt"),
            ConvergeSemanticStep::PublishedOne {
                state_after: SemanticCommitPlanState::Ready,
                ..
            }
        ));
        close_effect(&task, task_id, attempt_id, permit_id, permit_epoch);
        drop(task);

        let faulted = SqliteTaskAuthority::open_with_vfs(&fixture.task_path, Some(VFS_NAME))
            .expect("reopen Task authority through fault VFS");
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let result = SemanticCommitCoordinator::new(&faulted, &semantic)
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 });
        assert!(matches!(
            result,
            Err(nlos_commit_coordinator::CoordinatorError::Task(_))
        ));
        assert!(nlos_store_fault::writes_observed() > 0);
        drop(faulted);
        drop(semantic);
        nlos_store_fault::disarm();

        recover_pending(&fixture, plan_id, 10);
    }
}
