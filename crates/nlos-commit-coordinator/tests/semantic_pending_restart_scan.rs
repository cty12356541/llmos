//! Bounded restart-scan coverage for the Semantic cross-authority coordinator.
//!
//! The baseline coordinator tests exercise a single-plan `converge` call after
//! reopening the `TaskAuthority`.  This integration binary keeps the owner
//! publication prefix durable, drops the `TaskAuthority`, and proves that the
//! public bounded `converge_pending` entry point reconstructs the remaining
//! owner replay, Task receipt consumption, and terminal finalize without
//! caller-supplied plan data.

mod baseline {
    include!("semantic_convergence.rs");

    #[test]
    fn pending_restart_scan_replays_publishing_owner_prefix() {
        let fixture = Fixture::new();
        let (task, semantic, plan_id, ..) = prepare(&fixture, false);
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);

        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
                .unwrap(),
            ConvergeSemanticStep::Authorized
        ));
        let progress = task.inspect_semantic_commit_progress(plan_id).unwrap();
        assert_eq!(progress.plan.state, SemanticCommitPlanState::Publishing);
        let expectation = task
            .inspect_semantic_commit_expectations(plan_id)
            .unwrap()
            .into_iter()
            .next()
            .expect("fixture declares one Semantic publication");

        // Simulate the owner authority having committed its publication after
        // the Task-side transaction was interrupted. The coordinator must use
        // the exact sealed expectation and let the owner return its replay.
        let owner_receipt = semantic
            .publish_semantic_publication(nlos_semantic::PublishSemanticPublicationRequest {
                task_id: progress.plan.task_id,
                permit_id: progress.plan.permit_id,
                write_set_root: progress.plan.write_set_root,
                event_id: expectation.event_id,
                target: nlos_capability::CapabilityTarget::Namespace(NamespaceId::from_bytes(
                    [0xc0; 16],
                )),
                admission_receipt_id: expectation.admission_receipt_id,
                durability_receipt_id: expectation.durability_receipt_id,
                published_at_ms: 8,
            })
            .unwrap();
        assert_eq!(owner_receipt.receipt().event_id, expectation.event_id);
        drop(task);

        let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
        let receipts = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge_pending(1, 9)
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].semantic_publications.len(), 1);
        assert_eq!(
            receipts[0].semantic_publications[0].event_id,
            expectation.event_id
        );
        assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);
        assert!(matches!(
            reopened.inspect_semantic_commit_progress(plan_id),
            Ok(progress) if progress.plan.state == SemanticCommitPlanState::Finalized
        ));
        assert!(
            reopened
                .list_incomplete_semantic_commit_plans(1)
                .unwrap()
                .is_empty()
        );
    }
}
