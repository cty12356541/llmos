//! Single-Cell `CapacityLease` host attach.
//!
//! `TARGET_PREPARED → ACTIVE` lands only on an accepted `HostAttachReceipt`
//! whose id is the idempotency key and whose axes (target node, boot,
//! capacity epoch, fencing token, full `amount`) bind the committed lease.
//! Return from `ACTIVE` is symmetric with the pre-active path:
//! `ACTIVE → RETURNING → RETURNED`, with `RETURNED` refunding `amount` once —
//! capacity is returned whole, not consumed.
//!
//! In-memory pool only; no durable attach receipt store in this slice. One
//! `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellFencingToken};
use nlos_lease::{
    CapacityLeaseGrant, CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseReturnError,
    CapacityLeaseState, HostAttachReceipt,
};
use nlos_types::{CapacityLeaseId, ReceiptId, SchedulerDomainId};

#[test]
fn capacity_lease_host_attach_receipt_moves_prepared_to_active() {
    let authority = CellAuthority::claim(SchedulerDomainId::from_bytes([0xa4; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = CapacityLeaseGrantor::open(authority, 100);
    let lease_id = CapacityLeaseId::from_bytes([0xa6; 16]);

    let reserved = grantor.grant(&current, lease_id, 40).expect("reserve");
    assert_eq!(reserved.state(), CapacityLeaseState::GlobalReserved);
    assert_eq!(
        grantor.activate(
            &current,
            attach_for(ReceiptId::from_bytes([0xb1; 16]), &reserved)
        ),
        Err(CapacityLeaseReturnError::AttachRequiresTargetPrepared {
            state: CapacityLeaseState::GlobalReserved,
        })
    );
    grantor.prepare(&current, lease_id).expect("prepare");
    assert_eq!(
        grantor.query(lease_id).expect("query").state(),
        CapacityLeaseState::TargetPrepared
    );

    attach_rejects_before_receipt_is_accepted(&mut grantor, &current, &stale, &reserved);
    let active = attach_accepts_and_replays(&mut grantor, &current, &reserved);
    return_from_active_is_symmetric(&mut grantor, &current, lease_id, active);
}

fn attach_rejects_before_receipt_is_accepted(
    grantor: &mut CapacityLeaseGrantor,
    current: &CellFence,
    stale: &CellFence,
    prepared: &CapacityLeaseGrant,
) {
    assert_eq!(
        grantor.activate(
            stale,
            attach_for(ReceiptId::from_bytes([0xb2; 16]), prepared)
        ),
        Err(CapacityLeaseReturnError::Fence(
            CapacityLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );

    let partial = attach_with_amount(
        ReceiptId::from_bytes([0xb3; 16]),
        prepared,
        prepared.amount() - 5,
    );
    assert_eq!(
        grantor.activate(current, partial),
        Err(CapacityLeaseReturnError::AttachReceiptMismatch { receipt: partial })
    );

    let wrong_token = attach_with_token(
        ReceiptId::from_bytes([0xb4; 16]),
        prepared,
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        grantor.activate(current, wrong_token),
        Err(CapacityLeaseReturnError::AttachReceiptMismatch {
            receipt: wrong_token,
        })
    );

    let unknown = HostAttachReceipt::new(
        ReceiptId::from_bytes([0xb5; 16]),
        CapacityLeaseId::from_bytes([0xb6; 16]),
        prepared.target_node(),
        prepared.target_node_boot_generation(),
        prepared.capacity_epoch(),
        prepared.fencing_token(),
        prepared.amount(),
    );
    assert_eq!(
        grantor.activate(current, unknown),
        Err(CapacityLeaseReturnError::UnknownLease)
    );
    assert_eq!(
        grantor
            .query(prepared.capacity_lease_id())
            .expect("query")
            .state(),
        CapacityLeaseState::TargetPrepared
    );
    assert_eq!(grantor.pool_remaining(), 60);
    assert_eq!(grantor.attach_receipt(prepared.capacity_lease_id()), None);
}

fn attach_accepts_and_replays(
    grantor: &mut CapacityLeaseGrantor,
    current: &CellFence,
    prepared: &CapacityLeaseGrant,
) -> CapacityLeaseGrant {
    let receipt = attach_for(ReceiptId::from_bytes([0xb7; 16]), prepared);
    let active = grantor
        .activate(current, receipt)
        .expect("attach receipt lands ACTIVE");
    assert_eq!(active.state(), CapacityLeaseState::Active);
    assert_eq!(grantor.pool_remaining(), 60, "attach does not refund");
    assert_eq!(
        grantor.attach_receipt(prepared.capacity_lease_id()),
        Some(receipt)
    );

    assert_eq!(
        grantor
            .activate(current, receipt)
            .expect("identical receipt replays")
            .state(),
        CapacityLeaseState::Active
    );
    assert_eq!(
        grantor.attach_receipt(prepared.capacity_lease_id()),
        Some(receipt)
    );

    let same_key_wrong_amount =
        attach_with_amount(receipt.receipt_id(), prepared, prepared.amount() + 1);
    assert_eq!(
        grantor.activate(current, same_key_wrong_amount),
        Err(CapacityLeaseReturnError::ConflictingAttachReceipt {
            existing: receipt.receipt_id(),
            requested: same_key_wrong_amount.receipt_id(),
        })
    );

    let fresh_key = attach_for(ReceiptId::from_bytes([0xb8; 16]), prepared);
    assert_eq!(
        grantor.activate(current, fresh_key),
        Err(CapacityLeaseReturnError::ConflictingAttachReceipt {
            existing: receipt.receipt_id(),
            requested: fresh_key.receipt_id(),
        })
    );
    assert_eq!(
        grantor.prepare(current, prepared.capacity_lease_id()),
        Err(CapacityLeaseReturnError::NotGlobalReserved {
            state: CapacityLeaseState::Active,
        })
    );
    active
}

fn return_from_active_is_symmetric(
    grantor: &mut CapacityLeaseGrantor,
    current: &CellFence,
    lease_id: CapacityLeaseId,
    active: CapacityLeaseGrant,
) {
    assert_eq!(active.state(), CapacityLeaseState::Active);
    assert_eq!(
        grantor.ack_return(current, lease_id),
        Err(CapacityLeaseReturnError::NotReturning {
            state: CapacityLeaseState::Active,
        })
    );
    assert_eq!(grantor.pool_remaining(), 60);

    let returning = grantor
        .begin_return(current, lease_id)
        .expect("return from ACTIVE");
    assert_eq!(returning.state(), CapacityLeaseState::Returning);
    assert_eq!(grantor.pool_remaining(), 60, "RETURNING does not refund");

    let returned = grantor.ack_return(current, lease_id).expect("return ACK");
    assert_eq!(returned.state(), CapacityLeaseState::Returned);
    assert_eq!(grantor.pool_remaining(), 100, "RETURNED refunds once");
    assert_eq!(
        grantor
            .ack_return(current, lease_id)
            .expect("idempotent ACK")
            .state(),
        CapacityLeaseState::Returned
    );
    assert_eq!(grantor.pool_remaining(), 100);
    assert_eq!(
        grantor.begin_return(current, lease_id),
        Err(CapacityLeaseReturnError::NotReserved {
            state: CapacityLeaseState::Returned,
        })
    );
}

fn attach_for(receipt_id: ReceiptId, lease: &CapacityLeaseGrant) -> HostAttachReceipt {
    attach_with_amount(receipt_id, lease, lease.amount())
}

fn attach_with_amount(
    receipt_id: ReceiptId,
    lease: &CapacityLeaseGrant,
    attached_amount: u64,
) -> HostAttachReceipt {
    HostAttachReceipt::new(
        receipt_id,
        lease.capacity_lease_id(),
        lease.target_node(),
        lease.target_node_boot_generation(),
        lease.capacity_epoch(),
        lease.fencing_token(),
        attached_amount,
    )
}

fn attach_with_token(
    receipt_id: ReceiptId,
    lease: &CapacityLeaseGrant,
    fencing_token: CellFencingToken,
) -> HostAttachReceipt {
    HostAttachReceipt::new(
        receipt_id,
        lease.capacity_lease_id(),
        lease.target_node(),
        lease.target_node_boot_generation(),
        lease.capacity_epoch(),
        fencing_token,
        lease.amount(),
    )
}
