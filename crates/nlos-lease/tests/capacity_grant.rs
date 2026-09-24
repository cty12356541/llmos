//! Single-Cell `CapacityLease` grant: deduct then issue; stale epoch typed reject.
//!
//! `LEASE-CAPACITY-001` (in-memory source pool only): `amount` is deducted
//! into `GLOBAL_RESERVED` before the lease is issued. No durable second ledger,
//! no `TARGET_PREPARED` / `ACTIVE` / reclaim in this slice.
//!
//! Fence admit is [`nlos_cell::CellAuthority::admit`] (one claim per process).
//! One `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_lease::{CapacityLeaseGrantError, CapacityLeaseGrantor, CapacityLeaseState, FenceScope};
use nlos_types::{CapacityLeaseId, Generation, SchedulerDomainId};

#[test]
fn capacity_lease_grant_uses_cell_authority_admit_and_deducts_pool() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xd1; 16])).expect("claim");

    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    assert_eq!(stale.epoch(), CellEpoch::INITIAL);
    assert_eq!(
        current.epoch(),
        CellEpoch::INITIAL.checked_next().expect("epoch 2")
    );

    let mut grantor = CapacityLeaseGrantor::open(authority, 100);

    assert_eq!(
        grantor.grant(&stale, CapacityLeaseId::from_bytes([0xe2; 16]), 25),
        Err(CapacityLeaseGrantError::StaleEpoch {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        })
    );
    assert_eq!(grantor.pool_remaining(), 100);

    let lease_id = CapacityLeaseId::from_bytes([0xe1; 16]);
    let grant = grantor
        .grant(&current, lease_id, 40)
        .expect("current fence must grant");

    assert_eq!(grant.capacity_lease_id(), lease_id);
    assert_eq!(grant.target_node(), current.identity());
    assert_eq!(grant.target_node_boot_generation(), Generation::INITIAL);
    assert_eq!(grant.amount(), 40);
    assert_eq!(grant.capacity_epoch(), current.epoch());
    assert_eq!(grant.fencing_token(), current.fencing_token());
    assert_eq!(grant.fence_scope(), FenceScope::cell(current.identity()));
    assert_eq!(grant.state(), CapacityLeaseState::GlobalReserved);
    assert_eq!(grantor.pool_remaining(), 60);

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xd2; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        grantor.grant(&foreign, CapacityLeaseId::from_bytes([0xe3; 16]), 10),
        Err(CapacityLeaseGrantError::IdentityMismatch {
            presented: CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xd2; 16])),
            current: current.identity(),
        })
    );

    let wrong_boot = CellFence::present(
        current.identity(),
        Generation::INITIAL.checked_next().expect("boot 2"),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        grantor.grant(&wrong_boot, CapacityLeaseId::from_bytes([0xe4; 16]), 10),
        Err(CapacityLeaseGrantError::BootGenerationMismatch {
            presented: Generation::INITIAL.checked_next().expect("boot 2"),
            current: Generation::INITIAL,
        })
    );

    let wrong_token = CellFence::present(
        current.identity(),
        current.node_boot_generation(),
        current.epoch(),
        CellFencingToken::INITIAL,
    );
    assert_eq!(
        grantor.grant(&wrong_token, CapacityLeaseId::from_bytes([0xe5; 16]), 10),
        Err(CapacityLeaseGrantError::FencingTokenMismatch {
            presented: CellFencingToken::INITIAL,
            current: current.fencing_token(),
        })
    );
    assert_eq!(grantor.pool_remaining(), 60);

    assert_eq!(
        grantor.grant(&current, CapacityLeaseId::from_bytes([0xe6; 16]), 61),
        Err(CapacityLeaseGrantError::InsufficientCapacity {
            requested: 61,
            available: 60,
        })
    );
    assert_eq!(grantor.pool_remaining(), 60);
}
