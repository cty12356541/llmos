//! Single-Cell `CapacityLease` pre-active prepare.
//!
//! `LEASE-PREACTIVE-001`: `TARGET_PREPARED` has an idempotent return path.
//! `GLOBAL_RESERVED → TARGET_PREPARED` does not refund. Return from
//! `TARGET_PREPARED` still waits for `RETURNED` before the source pool moves.
//! This slice does not enter `ACTIVE` (no host attach receipt).
//!
//! In-memory pool only. One `#[test]` so this file claims the process-scoped
//! Cell only once.

use nlos_cell::{CellAuthority, CellEpoch};
use nlos_lease::{
    CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseReturnError, CapacityLeaseState,
};
use nlos_types::{CapacityLeaseId, SchedulerDomainId};

#[test]
fn capacity_lease_target_prepared_returns_without_refund_until_ack() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xc3; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = CapacityLeaseGrantor::open(authority, 80);
    let lease_id = CapacityLeaseId::from_bytes([0xc4; 16]);

    let reserved = grantor.grant(&current, lease_id, 30).expect("reserve");
    assert_eq!(reserved.state(), CapacityLeaseState::GlobalReserved);
    assert_eq!(grantor.pool_remaining(), 50);

    assert_eq!(
        grantor.prepare(&stale, lease_id),
        Err(CapacityLeaseReturnError::Fence(
            CapacityLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );
    assert_eq!(grantor.pool_remaining(), 50);

    let prepared = grantor.prepare(&current, lease_id).expect("prepare");
    assert_eq!(prepared.state(), CapacityLeaseState::TargetPrepared);
    assert_eq!(
        grantor.pool_remaining(),
        50,
        "prepare keeps the amount committed"
    );
    assert_eq!(
        grantor
            .prepare(&current, lease_id)
            .expect("idempotent prepare")
            .state(),
        CapacityLeaseState::TargetPrepared
    );
    assert_eq!(
        grantor.query(lease_id).expect("query").state(),
        CapacityLeaseState::TargetPrepared
    );

    let returning = grantor
        .begin_return(&current, lease_id)
        .expect("return from TARGET_PREPARED");
    assert_eq!(returning.state(), CapacityLeaseState::Returning);
    assert_eq!(grantor.pool_remaining(), 50, "RETURNING does not refund");

    let returned = grantor.ack_return(&current, lease_id).expect("return ACK");
    assert_eq!(returned.state(), CapacityLeaseState::Returned);
    assert_eq!(grantor.pool_remaining(), 80);
    assert_eq!(
        grantor.prepare(&current, lease_id),
        Err(CapacityLeaseReturnError::NotGlobalReserved {
            state: CapacityLeaseState::Returned,
        })
    );
    assert_eq!(grantor.pool_remaining(), 80);
}
