//! Bounded restart-scan coverage for a mixed Effect + Semantic `TaskWriteSet`.
//!
//! This is deliberately local evidence: the mixed-finalize envelope and both
//! authority records already exist durably before the scan. The test proves
//! that restart recovery needs no caller-supplied effect proof or Semantic
//! binding, without claiming a distributed atomic transaction.

mod baseline {
    include!("semantic_convergence.rs");

    #[test]
    fn pending_restart_scan_reconstructs_mixed_finalize_envelope() {
        let fixture = Fixture::new();
        let (task, semantic, plan_id, task_id, attempt_id, permit_id, permit_epoch) =
            prepare(&fixture, true);
        task.prepare_semantic_finalize(nlos_task::PrepareSemanticFinalizeRequest {
            plan_id,
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
            prepared_at_ms: 6,
        })
        .unwrap();
        let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
                .unwrap(),
            ConvergeSemanticStep::Authorized
        ));
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 })
                .unwrap(),
            ConvergeSemanticStep::PublishedOne {
                state_after: SemanticCommitPlanState::Ready,
                ..
            }
        ));

        // Close the optional Effect slot before the restart. The persisted
        // envelope then contains all local proof needed by the v3 finalize
        // path; the pending scan must reconstruct that path from Task data.
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
            .unwrap()
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
        .unwrap();
        drop(task);

        let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
        let receipts = SemanticCommitCoordinator::new(&reopened, &semantic)
            .converge_pending(1, 11)
            .unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].semantic_publications.len(), 1);
        assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);
        assert!(matches!(
            reopened.inspect_semantic_commit_progress(plan_id),
            Ok(progress) if progress.plan.state == SemanticCommitPlanState::Finalized
        ));
        assert!(
            reopened
                .inspect_semantic_finalize_envelope(plan_id)
                .unwrap()
                .is_some()
        );
        assert!(
            reopened
                .list_incomplete_semantic_commit_plans(1)
                .unwrap()
                .is_empty()
        );
    }
}
