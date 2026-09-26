//! Single-Cell `CapacityLease` pre-active return.
//!
//! `LEASE-PREACTIVE-001`: `GLOBAL_RESERVED` has an idempotent return path.
//! `GLOBAL_RESERVED → RETURNING` does not put `amount` back in the source
//! pool. `RETURNING → RETURNED` refunds once. This slice has no `ACTIVE`
//! state, so a reserved lease is the proof it was never activated.
//!
//! In-memory pool only. No durable attach receipt. One `#[test]` so this file
//! claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch};
use nlos_lease::{
    CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseReturnError, CapacityLeaseState,
};
use nlos_types::{CapacityLeaseId, SchedulerDomainId};

#[test]
fn capacity_lease_preactive_return_refunds_once_on_ack() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xa1; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = CapacityLeaseGrantor::open(authority, 100);
    let lease_id = CapacityLeaseId::from_bytes([0xb1; 16]);

    let reserved = grantor
        .grant(&current, lease_id, 40)
        .expect("reserve capacity");
    assert_eq!(reserved.state(), CapacityLeaseState::GlobalReserved);
    assert_eq!(grantor.pool_remaining(), 60);
    assert_eq!(
        grantor.query(lease_id).expect("query").state(),
        CapacityLeaseState::GlobalReserved
    );
    assert_eq!(
        grantor.grant(&current, lease_id, 40).expect("replay"),
        reserved
    );
    assert_eq!(grantor.pool_remaining(), 60);

    assert_eq!(
        grantor.begin_return(&stale, lease_id),
        Err(CapacityLeaseReturnError::Fence(
            CapacityLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );
    assert_eq!(grantor.pool_remaining(), 60);

    let returning = grantor
        .begin_return(&current, lease_id)
        .expect("begin return");
    assert_eq!(returning.state(), CapacityLeaseState::Returning);
    assert_eq!(grantor.pool_remaining(), 60, "RETURNING does not refund");
    assert_eq!(
        grantor
            .begin_return(&current, lease_id)
            .expect("idempotent begin")
            .state(),
        CapacityLeaseState::Returning
    );

    let returned = grantor.ack_return(&current, lease_id).expect("return ACK");
    assert_eq!(returned.state(), CapacityLeaseState::Returned);
    assert_eq!(grantor.pool_remaining(), 100);
    assert_eq!(
        grantor
            .ack_return(&current, lease_id)
            .expect("idempotent ACK")
            .state(),
        CapacityLeaseState::Returned
    );
    assert_eq!(grantor.pool_remaining(), 100);

    let second = CapacityLeaseId::from_bytes([0xb2; 16]);
    grantor.grant(&current, second, 10).expect("second reserve");
    assert_eq!(grantor.pool_remaining(), 90);
    assert_eq!(
        grantor.ack_return(&current, second),
        Err(CapacityLeaseReturnError::NotReturning {
            state: CapacityLeaseState::GlobalReserved,
        })
    );
    assert_eq!(grantor.pool_remaining(), 90);
    grantor
        .begin_return(&current, second)
        .expect("begin second");
    grantor.ack_return(&current, second).expect("ack second");
    assert_eq!(grantor.pool_remaining(), 100);
}
