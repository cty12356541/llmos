//! Single-Cell `ExclusiveDeviceLease` reset-gated return.
//!
//! `LEASE-DEVICE-001` return leg: `DEVICE_RESERVED → RESETTING` declares the
//! holder's reset+zeroization intent; the holder then submits a
//! `DeviceResetReceipt` (`RESETTING → RETURNED`) whose id is the idempotency
//! key and whose `reset_generation` fences older head eras. Only an accepted
//! receipt moves the `DeviceLeaseHead` back to FREE and bumps the head
//! generation. Accepted receipts append to an immutable log.
//!
//! In-memory log only; no durable receipt store in this slice. One `#[test]`
//! so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_lease::{
    DeviceResetReceipt, ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseGrantError,
    ExclusiveDeviceLeaseGrantor, ExclusiveDeviceLeaseReturnError, ExclusiveDeviceLeaseState,
};
use nlos_types::{DeviceId, ExclusiveDeviceLeaseId, Generation, ReceiptId, SchedulerDomainId};

#[test]
fn exclusive_device_lease_return_unblocks_only_on_reset_receipt() {
    let authority = CellAuthority::claim(SchedulerDomainId::from_bytes([0xa5; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");

    let device_id = DeviceId::from_bytes([0xa6; 16]);
    let mut grantor = ExclusiveDeviceLeaseGrantor::open(authority, device_id);

    let lease_id = ExclusiveDeviceLeaseId::from_bytes([0xa7; 16]);
    let grant = grantor.grant(&current, lease_id).expect("grant");
    assert_eq!(grant.state(), ExclusiveDeviceLeaseState::DeviceReserved);
    assert_eq!(grant.reset_generation(), Generation::INITIAL);
    assert!(!grantor.is_free());
    assert_eq!(
        grantor.grant(&current, lease_id).expect("same id replays"),
        grant
    );

    receipt_before_declare_is_refused(&mut grantor, &current, &grant);
    declare_moves_to_resetting(&mut grantor, &current, &stale, lease_id);
    generation_and_bind_fences_are_fail_closed(&mut grantor, &current, &grant);
    accepted_receipt_returns_the_head(&mut grantor, &current, &grant);
    replay_and_conflicts_after_return(&mut grantor, &current, &grant);
    second_era_fences_old_generation(&mut grantor, &current);
}

fn receipt_before_declare_is_refused(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
    grant: &ExclusiveDeviceLeaseGrant,
) {
    let eager = receipt_for(ReceiptId::from_bytes([0xb1; 16]), grant);
    assert_eq!(
        grantor.submit_reset_receipt(current, eager),
        Err(ExclusiveDeviceLeaseReturnError::ReceiptRequiresResetting {
            state: ExclusiveDeviceLeaseState::DeviceReserved,
        })
    );
    assert!(!grantor.is_free());
}

fn declare_moves_to_resetting(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
    stale: &CellFence,
    lease_id: ExclusiveDeviceLeaseId,
) {
    assert_eq!(
        grantor.declare_reset(stale, lease_id),
        Err(ExclusiveDeviceLeaseReturnError::Fence(
            ExclusiveDeviceLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );
    assert!(!grantor.is_free());

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xa8; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    let committed = grantor.query(lease_id).expect("query");
    assert_eq!(
        grantor.submit_reset_receipt(
            &foreign,
            receipt_for(ReceiptId::from_bytes([0xb2; 16]), &committed)
        ),
        Err(ExclusiveDeviceLeaseReturnError::Fence(
            ExclusiveDeviceLeaseGrantError::IdentityMismatch {
                presented: CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xa8; 16])),
                current: current.identity(),
            }
        ))
    );

    let declared = grantor
        .declare_reset(current, lease_id)
        .expect("declare reset");
    assert_eq!(declared.state(), ExclusiveDeviceLeaseState::Resetting);
    assert!(!grantor.is_free());
    assert_eq!(
        grantor
            .declare_reset(current, lease_id)
            .expect("idempotent declare")
            .state(),
        ExclusiveDeviceLeaseState::Resetting
    );
    assert_eq!(
        grantor.declare_reset(current, ExclusiveDeviceLeaseId::from_bytes([0xa9; 16])),
        Err(ExclusiveDeviceLeaseReturnError::UnknownLease)
    );
}

fn generation_and_bind_fences_are_fail_closed(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
    grant: &ExclusiveDeviceLeaseGrant,
) {
    let future_generation = grant
        .reset_generation()
        .checked_next()
        .expect("future generation");
    let future_era =
        receipt_with_generation(ReceiptId::from_bytes([0xb3; 16]), grant, future_generation);
    assert_eq!(
        grantor.submit_reset_receipt(current, future_era),
        Err(ExclusiveDeviceLeaseReturnError::StaleResetGeneration {
            receipt: future_generation,
            head: grant.reset_generation(),
        })
    );
    assert!(!grantor.is_free());

    let wrong_token = receipt_with_token(
        ReceiptId::from_bytes([0xb2; 16]),
        grant,
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        grantor.submit_reset_receipt(current, wrong_token),
        Err(ExclusiveDeviceLeaseReturnError::ReceiptBindMismatch {
            receipt: wrong_token,
        })
    );
    assert!(!grantor.is_free());
}

fn accepted_receipt_returns_the_head(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
    grant: &ExclusiveDeviceLeaseGrant,
) {
    let receipt = receipt_for(ReceiptId::from_bytes([0xb4; 16]), grant);
    let returned = grantor
        .submit_reset_receipt(current, receipt)
        .expect("receipt unblocks the return");
    assert_eq!(returned.state(), ExclusiveDeviceLeaseState::Returned);
    assert!(grantor.is_free());
    assert_eq!(grantor.reset_generation(), head_generation_2());
    assert_eq!(grantor.reset_receipts(), &[receipt]);
    assert_eq!(
        grantor
            .query(grant.device_lease_id())
            .expect("query")
            .state(),
        ExclusiveDeviceLeaseState::Returned
    );
}

fn replay_and_conflicts_after_return(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
    grant: &ExclusiveDeviceLeaseGrant,
) {
    let receipt = receipt_for(ReceiptId::from_bytes([0xb4; 16]), grant);
    assert_eq!(
        grantor
            .submit_reset_receipt(current, receipt)
            .expect("identical receipt replays")
            .state(),
        ExclusiveDeviceLeaseState::Returned
    );
    assert_eq!(grantor.reset_receipts(), &[receipt]);
    assert!(grantor.is_free());

    let same_key_wrong_token =
        receipt_with_token(receipt.receipt_id(), grant, CellFencingToken::INITIAL);
    assert_eq!(
        grantor.submit_reset_receipt(current, same_key_wrong_token),
        Err(ExclusiveDeviceLeaseReturnError::ConflictingResetReceipt {
            existing: receipt.receipt_id(),
            requested: same_key_wrong_token.receipt_id(),
        })
    );

    let fresh_key = receipt_for(ReceiptId::from_bytes([0xb5; 16]), grant);
    assert_eq!(
        grantor.submit_reset_receipt(current, fresh_key),
        Err(ExclusiveDeviceLeaseReturnError::ConflictingResetReceipt {
            existing: receipt.receipt_id(),
            requested: fresh_key.receipt_id(),
        })
    );
    assert_eq!(
        grantor.declare_reset(current, grant.device_lease_id()),
        Err(ExclusiveDeviceLeaseReturnError::DeclareRequiresReserved {
            state: ExclusiveDeviceLeaseState::Returned,
        })
    );
    assert_eq!(
        grantor
            .grant(current, grant.device_lease_id())
            .expect("returned id replays, does not re-claim")
            .state(),
        ExclusiveDeviceLeaseState::Returned
    );
    assert!(grantor.is_free());
}

fn second_era_fences_old_generation(
    grantor: &mut ExclusiveDeviceLeaseGrantor,
    current: &CellFence,
) {
    let second_id = ExclusiveDeviceLeaseId::from_bytes([0xba; 16]);
    let second = grantor.grant(current, second_id).expect("second grant");
    assert_eq!(second.reset_generation(), head_generation_2());
    assert!(!grantor.is_free());
    grantor
        .declare_reset(current, second_id)
        .expect("declare second");

    let older_era = receipt_with_generation(
        ReceiptId::from_bytes([0xbb; 16]),
        &second,
        Generation::INITIAL,
    );
    assert_eq!(
        grantor.submit_reset_receipt(current, older_era),
        Err(ExclusiveDeviceLeaseReturnError::StaleResetGeneration {
            receipt: Generation::INITIAL,
            head: head_generation_2(),
        })
    );
    assert!(!grantor.is_free());

    let wrong_token = receipt_with_token(
        ReceiptId::from_bytes([0xbc; 16]),
        &second,
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        grantor.submit_reset_receipt(current, wrong_token),
        Err(ExclusiveDeviceLeaseReturnError::ReceiptBindMismatch {
            receipt: wrong_token,
        })
    );

    let unknown_lease = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0xbd; 16]),
        ExclusiveDeviceLeaseId::from_bytes([0xbe; 16]),
        second.holder_node(),
        second.holder_node_boot_generation(),
        second.exclusivity_epoch(),
        token_2(),
        second.reset_generation(),
    );
    assert_eq!(
        grantor.submit_reset_receipt(current, unknown_lease),
        Err(ExclusiveDeviceLeaseReturnError::UnknownLease)
    );
    assert_eq!(
        grantor.grant(current, ExclusiveDeviceLeaseId::from_bytes([0xc0; 16])),
        Err(ExclusiveDeviceLeaseGrantError::DeviceNotFree {
            device_id: second.device_id(),
        })
    );

    let second_receipt = receipt_for(ReceiptId::from_bytes([0xbf; 16]), &second);
    let returned = grantor
        .submit_reset_receipt(current, second_receipt)
        .expect("second return");
    assert_eq!(returned.state(), ExclusiveDeviceLeaseState::Returned);
    assert!(grantor.is_free());
    assert_eq!(
        grantor.reset_generation(),
        head_generation_2()
            .checked_next()
            .expect("head generation 3")
    );
    assert_eq!(grantor.reset_receipts().len(), 2);
    assert_eq!(
        grantor.reset_receipts()[1].receipt_id(),
        second_receipt.receipt_id()
    );
}

fn head_generation_2() -> Generation {
    Generation::INITIAL.checked_next().expect("generation 2")
}

fn token_2() -> CellFencingToken {
    CellFencingToken::INITIAL.checked_next().expect("token 2")
}

fn receipt_for(receipt_id: ReceiptId, lease: &ExclusiveDeviceLeaseGrant) -> DeviceResetReceipt {
    receipt_with_generation(receipt_id, lease, lease.reset_generation())
}

fn receipt_with_generation(
    receipt_id: ReceiptId,
    lease: &ExclusiveDeviceLeaseGrant,
    reset_generation: Generation,
) -> DeviceResetReceipt {
    DeviceResetReceipt::new(
        receipt_id,
        lease.device_lease_id(),
        lease.holder_node(),
        lease.holder_node_boot_generation(),
        lease.exclusivity_epoch(),
        lease.fencing_token(),
        reset_generation,
    )
}

fn receipt_with_token(
    receipt_id: ReceiptId,
    lease: &ExclusiveDeviceLeaseGrant,
    fencing_token: CellFencingToken,
) -> DeviceResetReceipt {
    DeviceResetReceipt::new(
        receipt_id,
        lease.device_lease_id(),
        lease.holder_node(),
        lease.holder_node_boot_generation(),
        lease.exclusivity_epoch(),
        fencing_token,
        lease.reset_generation(),
    )
}
