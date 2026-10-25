//! Integration tests for Cell-local partition autonomy (`[DIST-LOCAL-001]`) and
//! explicit remote call failure / retry / uncertainty contracts
//! (`[DIST-FAIL-001]`).

use std::num::NonZeroU64;
use std::sync::{Arc, OnceLock};

use nlos_cell::{
    CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity, ControlPlaneLinkPhase,
    ControlPlaneLinkState, LocalAutonomyProof, PartitionAutonomyGate, PartitionViolation,
    PartitionedActionKind, RemoteCallContract, RemoteCallOutcome, RemoteRetryDirective,
    RemoteRetryRefusal,
};
use nlos_types::{Generation, IdempotencyKey, SchedulerDomainId};

fn shared_process_authority() -> &'static Arc<CellAuthority> {
    static AUTHORITY: OnceLock<Arc<CellAuthority>> = OnceLock::new();
    AUTHORITY.get_or_init(|| {
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xA1; 16]))
            .expect("claim unique process CellAuthority")
            .into_shared()
    })
}

fn synthetic_fence(id_byte: u8, boot_gen: u64, epoch: u64, token: u64) -> CellFence {
    CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([id_byte; 16])),
        Generation::new(NonZeroU64::new(boot_gen).expect("non-zero boot_gen")),
        CellEpoch::from_u64(epoch).expect("non-zero epoch"),
        CellFencingToken::from_u64(token).expect("non-zero token"),
    )
}

#[test]
fn connected_to_partitioned_to_reconnecting_to_connected_state_machine() {
    let authority = shared_process_authority();
    let initial_fence = authority.fence();
    let mut gate = PartitionAutonomyGate::new_connected(initial_fence, 100);
    assert!(gate.state().is_connected());
    assert!(!gate.state().requires_local_autonomy_gate());

    // Regressing tick is rejected fail-closed.
    assert_eq!(
        gate.record_control_plane_contact(initial_fence, 99),
        Err(PartitionViolation::NonMonotonicTick {
            previous_tick: 100,
            supplied_tick: 99,
        })
    );

    // Refresh healthy contact at tick 120.
    let connected = gate
        .record_control_plane_contact(initial_fence, 120)
        .expect("refresh connected contact");
    assert_eq!(
        connected,
        ControlPlaneLinkState::Connected {
            verified_fence: initial_fence,
            last_contact_tick: 120,
        }
    );

    // Cut control plane into Partitioned at tick 150.
    let partitioned = gate
        .enter_partition(initial_fence, 150, 0x0503)
        .expect("enter partition");
    assert_eq!(
        partitioned,
        ControlPlaneLinkState::Partitioned {
            frozen_fence: initial_fence,
            partitioned_since_tick: 150,
            reason_code: 0x0503,
        }
    );
    assert!(gate.state().is_partitioned());
    assert!(gate.state().requires_local_autonomy_gate());

    // Idempotent duplicate partition cut preserves partitioned_since_tick.
    let duplicate_cut = gate
        .enter_partition(initial_fence, 160, 0x0503)
        .expect("idempotent partition cut");
    assert_eq!(duplicate_cut, partitioned);

    // Reconnection without advancing epoch/fencing_token MUST fail closed.
    assert_eq!(
        gate.begin_reconnection(initial_fence, 170),
        Err(PartitionViolation::ReconnectingEpochNotAdvanced {
            prior_fence: initial_fence,
            candidate_fence: initial_fence,
        })
    );

    // Advance real CellAuthority epoch -> begin reconnection -> complete reconnection.
    let reconnected_fence = authority.advance_epoch().expect("advance epoch");
    let reconnecting = gate
        .begin_reconnection(reconnected_fence, 180)
        .expect("begin reconnection");
    assert_eq!(
        reconnecting,
        ControlPlaneLinkState::Reconnecting {
            prior_frozen_fence: initial_fence,
            candidate_fence: reconnected_fence,
            started_tick: 180,
        }
    );
    assert!(gate.state().is_reconnecting());
    assert!(gate.state().requires_local_autonomy_gate());

    let healed = gate
        .complete_reconnection(reconnected_fence, 200)
        .expect("complete reconnection");
    assert_eq!(
        healed,
        ControlPlaneLinkState::Connected {
            verified_fence: reconnected_fence,
            last_contact_tick: 200,
        }
    );
    assert!(gate.state().is_connected());
}

#[test]
fn partitioned_cell_admits_pure_compute_and_authorized_local_spend() {
    let fence_e1 = synthetic_fence(0xB1, 1, 1, 1);
    let mut gate = PartitionAutonomyGate::new_connected(fence_e1, 10);
    gate.enter_partition(fence_e1, 20, 42)
        .expect("enter partition");

    // 1. Pure local compute (requested_spend == 0) inside valid capability & lease.
    let pure_compute = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(300), 100, 0),
    );
    let receipt_compute = gate
        .admit_action(50, &fence_e1, &pure_compute)
        .expect("pure compute admitted during partition");
    assert_eq!(receipt_compute.authorized_spend(), 0);
    assert_eq!(receipt_compute.remaining_after_spend(), 100);
    assert!(receipt_compute.verify_digest());

    // 2. Local authorized spend strictly within remaining frozen QuotaLease balance.
    let authorized_spend = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(300), 100, 40),
    );
    let receipt_spend = gate
        .admit_action(60, &fence_e1, &authorized_spend)
        .expect("authorized local spend admitted during partition");
    assert_eq!(receipt_spend.authorized_spend(), 40);
    assert_eq!(receipt_spend.remaining_after_spend(), 60);
    assert_eq!(receipt_spend.verified_fence(), fence_e1);
    assert!(receipt_spend.verify_digest());

    // 3. Exact boundary spend (requested_spend == lease_remaining, now_tick == capability_valid_until_tick).
    let boundary_spend = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(299, false, true, fence_e1.epoch(), Some(300), 60, 60),
    );
    let receipt_boundary = gate
        .admit_action(299, &fence_e1, &boundary_spend)
        .expect("exact boundary local spend admitted");
    assert_eq!(receipt_boundary.authorized_spend(), 60);
    assert_eq!(receipt_boundary.remaining_after_spend(), 0);
    assert!(receipt_boundary.verify_digest());
}

#[test]
fn partitioned_cell_rejects_expired_unverified_or_overdrawn_local_proofs() {
    let fence_e1 = synthetic_fence(0xC2, 1, 2, 2);
    let gate = PartitionAutonomyGate::new_partitioned(fence_e1, 100, 7);

    // 1. Unverified capability -> CapabilityNotLocallyProvable.
    let unverified = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, false, fence_e1.epoch(), Some(400), 50, 10),
    );
    assert_eq!(
        gate.admit_action(150, &fence_e1, &unverified),
        Err(PartitionViolation::CapabilityNotLocallyProvable {
            capability_locally_verified: false,
            capability_revoked: false,
        })
    );

    // 2. Locally revoked capability -> CapabilityNotLocallyProvable.
    let revoked = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, true, true, fence_e1.epoch(), Some(400), 50, 10),
    );
    assert_eq!(
        gate.admit_action(150, &fence_e1, &revoked),
        Err(PartitionViolation::CapabilityNotLocallyProvable {
            capability_locally_verified: true,
            capability_revoked: true,
        })
    );

    // 3. Expired capability (now_tick > capability_valid_until_tick) -> CapabilityExpiredDuringPartition.
    let expired_cap = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(200, false, true, fence_e1.epoch(), Some(400), 50, 10),
    );
    assert_eq!(
        gate.admit_action(201, &fence_e1, &expired_cap),
        Err(PartitionViolation::CapabilityExpiredDuringPartition {
            now_tick: 201,
            valid_until_tick: 200,
        })
    );

    // 4. Stale/mismatched lease epoch -> LeaseEpochMismatchDuringPartition.
    let stale_lease_epoch = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, CellEpoch::INITIAL, Some(400), 50, 10),
    );
    assert_eq!(
        gate.admit_action(150, &fence_e1, &stale_lease_epoch),
        Err(PartitionViolation::LeaseEpochMismatchDuringPartition {
            lease_epoch: CellEpoch::INITIAL,
            expected_epoch: fence_e1.epoch(),
        })
    );

    // 5. Expired QuotaLease TTL (now_tick >= expires_at_tick) -> QuotaLeaseExpiredDuringPartition.
    let expired_lease = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(300), 50, 10),
    );
    assert_eq!(
        gate.admit_action(300, &fence_e1, &expired_lease),
        Err(PartitionViolation::QuotaLeaseExpiredDuringPartition {
            now_tick: 300,
            expires_at_tick: 300,
        })
    );

    // 6. Requested spend > frozen remaining -> QuotaFrozenInsufficientRemaining.
    let overdrawn = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(400), 25, 26),
    );
    assert_eq!(
        gate.admit_action(150, &fence_e1, &overdrawn),
        Err(PartitionViolation::QuotaFrozenInsufficientRemaining {
            requested_spend: 26,
            lease_remaining: 25,
        })
    );

    // 7. Mismatched presented fence -> FenceMismatchDuringPartition.
    let foreign_fence = synthetic_fence(0xD3, 1, 2, 2);
    let valid_proof = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(400), 25, 5),
    );
    assert_eq!(
        gate.admit_action(150, &foreign_fence, &valid_proof),
        Err(PartitionViolation::FenceMismatchDuringPartition {
            presented: foreign_fence,
            expected: fence_e1,
        })
    );
}

#[test]
fn control_plane_actions_are_blocked_in_partitioned_and_reconnecting_states() {
    let fence_e1 = synthetic_fence(0xE4, 1, 1, 1);
    let fence_e2 = synthetic_fence(0xE4, 1, 2, 2);
    let mut gate = PartitionAutonomyGate::new_connected(fence_e1, 10);

    let forbidden_actions = [
        PartitionedActionKind::RequestQuotaExpansionOrRenewal,
        PartitionedActionKind::CrossCellCanonicalPublishWithoutLocalAuthority,
        PartitionedActionKind::UncachedRemoteAuthorityMutation,
    ];

    // Admitted while Connected.
    for action in &forbidden_actions {
        let receipt = gate
            .admit_action(15, &fence_e1, action)
            .expect("control-plane action permitted while connected");
        assert!(receipt.verify_digest());
    }

    // Blocked while Partitioned.
    let part_state = gate.enter_partition(fence_e1, 20, 1).expect("partition");
    assert_eq!(part_state.phase(), ControlPlaneLinkPhase::Partitioned);
    for action in &forbidden_actions {
        assert_eq!(
            gate.admit_action(25, &fence_e1, action),
            Err(PartitionViolation::ControlPlaneRequiredDuringPartition {
                action: action.label(),
                link_phase: ControlPlaneLinkPhase::Partitioned,
            })
        );
    }

    // Blocked while Reconnecting, AND old-epoch local lease spend is also blocked!
    let recon_state = gate
        .begin_reconnection(fence_e2, 30)
        .expect("begin reconnection");
    assert_eq!(recon_state.phase(), ControlPlaneLinkPhase::Reconnecting);
    for action in &forbidden_actions {
        assert_eq!(
            gate.admit_action(35, &fence_e2, action),
            Err(PartitionViolation::ControlPlaneRequiredDuringPartition {
                action: action.label(),
                link_phase: ControlPlaneLinkPhase::Reconnecting,
            })
        );
    }

    let stale_epoch_spend = PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(
        LocalAutonomyProof::new(500, false, true, fence_e1.epoch(), Some(400), 50, 5),
    );
    assert_eq!(
        gate.admit_action(35, &fence_e2, &stale_epoch_spend),
        Err(PartitionViolation::LeaseEpochMismatchDuringPartition {
            lease_epoch: fence_e1.epoch(),
            expected_epoch: fence_e2.epoch(),
        })
    );
}

#[test]
fn remote_call_contract_enforces_explicit_failure_and_retry_semantics() {
    let key = IdempotencyKey::from_bytes([0x77; 16]);
    let call_id = [0x11; 16];

    // 1. Pre-dispatch Unavailable allows bounded retry with the EXACT SAME IdempotencyKey.
    let attempt0 = RemoteCallContract::new(call_id, key, 100, 2, 0, true);
    let out0: RemoteCallOutcome<u64> = attempt0.classify_pre_dispatch_unavailable();
    assert_eq!(
        out0,
        RemoteCallOutcome::Unavailable {
            deadline_tick: 100,
            retry_directive: RemoteRetryDirective::SafeToRetryWithSameKey,
            idempotency_key: key,
        }
    );
    assert!(!out0.is_uncertain());
    assert!(!out0.requires_reconciliation());

    let attempt1 = attempt0
        .next_retry_for_outcome(&out0, 200)
        .expect("retry 1 allowed");
    assert_eq!(attempt1.attempt_number, 1);
    assert_eq!(attempt1.idempotency_key, key);
    assert_eq!(attempt1.call_id, call_id);

    let out1: RemoteCallOutcome<u64> = attempt1.classify_pre_dispatch_unavailable();
    let attempt2 = attempt1
        .next_retry_for_outcome(&out1, 300)
        .expect("retry 2 allowed");
    assert_eq!(attempt2.attempt_number, 2);
    assert!(!attempt2.has_retry_budget_remaining());

    // Budget exhausted on attempt 2 (max_retries == 2).
    let out2: RemoteCallOutcome<u64> = attempt2.classify_pre_dispatch_unavailable();
    assert_eq!(
        out2.retry_directive(),
        Some(RemoteRetryDirective::DoNotRetryBudgetExhausted)
    );
    assert_eq!(
        attempt2.next_retry_for_outcome(&out2, 400),
        Err(RemoteRetryRefusal::RetryBudgetExhausted {
            attempt_number: 2,
            max_retries: 2,
        })
    );

    // 2. Side-effecting in-flight timeout MUST surface DeadlineExceededUncertain + ReconcileBeforeRetry,
    //    and MUST refuse blind retry (`[DIST-FAIL-001]`).
    let mutating_call = RemoteCallContract::new([0x22; 16], key, 500, 3, 0, true);
    let uncertain: RemoteCallOutcome<u64> = mutating_call.classify_inflight_timeout(525);
    assert!(uncertain.is_uncertain());
    assert!(uncertain.requires_reconciliation());
    assert_eq!(
        uncertain,
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick: 500,
            elapsed_ticks: 525,
            side_effecting: true,
            retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
            idempotency_key: key,
        }
    );
    assert_eq!(
        mutating_call.next_retry_for_outcome(&uncertain, 600),
        Err(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: key
            }
        )
    );

    // 3. Read-only in-flight timeout may retry with the same key while budget remains.
    let readonly_call = RemoteCallContract::new([0x33; 16], key, 500, 1, 0, false);
    let readonly_timeout: RemoteCallOutcome<u64> = readonly_call.classify_inflight_timeout(510);
    assert_eq!(
        readonly_timeout.retry_directive(),
        Some(RemoteRetryDirective::SafeToRetryWithSameKey)
    );
    let readonly_retry = readonly_call
        .next_retry_for_outcome(&readonly_timeout, 650)
        .expect("read-only retry allowed");
    let succeeded = readonly_retry.classify_succeeded(42_u64, CellEpoch::INITIAL, true);
    assert!(succeeded.is_succeeded());
    assert_eq!(
        readonly_retry.next_retry_for_outcome(&succeeded, 700),
        Err(RemoteRetryRefusal::AlreadySucceeded)
    );

    // 4. Multi-shard Partial outcome produces deterministic domain-separated uncertainty digest.
    let shard_call = RemoteCallContract::new([0x44; 16], key, 800, 2, 0, true);
    let partial_idempotent =
        shard_call.classify_partial(3, 1, Some(300_u64), true, b"shard-3-timeout");
    assert!(partial_idempotent.is_uncertain());
    assert_eq!(
        partial_idempotent.retry_directive(),
        Some(RemoteRetryDirective::RetryFailedShardsOnly)
    );
    let shard_retry = shard_call
        .next_retry_for_outcome(&partial_idempotent, 900)
        .expect("idempotent shard retry allowed");
    assert_eq!(shard_retry.attempt_number, 1);

    let partial_non_idempotent =
        shard_call.classify_partial(3, 1, Some(300_u64), false, b"shard-3-timeout");
    assert!(partial_non_idempotent.requires_reconciliation());
    assert_eq!(
        shard_call.next_retry_for_outcome(&partial_non_idempotent, 900),
        Err(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: key
            }
        )
    );
}
