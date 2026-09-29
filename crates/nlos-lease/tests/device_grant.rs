//! Single-Cell `ExclusiveDeviceLease` grant: reserve then issue; stale epoch typed reject.
//!
//! `LEASE-DEVICE-001` (in-memory `DeviceLeaseHead` FREE→reserved only): the device
//! is claimed into `DEVICE_RESERVED` before the lease is issued. No durable
//! second ledger, no `HOLDER_PREPARED` / `ACTIVE` / reset/zeroization in this
//! slice.
//!
//! Fence admit is [`nlos_cell::CellAuthority::admit`] (one claim per process).
//! One `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_lease::{
    ExclusiveDeviceLeaseGrantError, ExclusiveDeviceLeaseGrantor, ExclusiveDeviceLeaseState,
    FenceScope,
};
use nlos_types::{DeviceId, ExclusiveDeviceLeaseId, Generation, SchedulerDomainId};

#[test]
fn exclusive_device_lease_grant_uses_cell_authority_admit_and_reserves_free_device() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xd1; 16])).expect("claim");

    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    assert_eq!(stale.epoch(), CellEpoch::INITIAL);
    assert_eq!(
        current.epoch(),
        CellEpoch::INITIAL.checked_next().expect("epoch 2")
    );

    let device_id = DeviceId::from_bytes([0xf0; 16]);
    let mut grantor = ExclusiveDeviceLeaseGrantor::open(authority, device_id);
    assert!(grantor.is_free());

    assert_eq!(
        grantor.grant(&stale, ExclusiveDeviceLeaseId::from_bytes([0xf2; 16])),
        Err(ExclusiveDeviceLeaseGrantError::StaleEpoch {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        })
    );
    assert!(grantor.is_free());

    let lease_id = ExclusiveDeviceLeaseId::from_bytes([0xf1; 16]);
    let grant = grantor
        .grant(&current, lease_id)
        .expect("current fence must grant");

    assert_eq!(grant.device_lease_id(), lease_id);
    assert_eq!(grant.device_id(), device_id);
    assert_eq!(grant.holder_node(), current.identity());
    assert_eq!(grant.holder_node_boot_generation(), Generation::INITIAL);
    assert_eq!(grant.exclusivity_epoch(), current.epoch());
    assert_eq!(grant.fencing_token(), current.fencing_token());
    assert_eq!(grant.fence_scope(), FenceScope::cell(current.identity()));
    assert_eq!(grant.state(), ExclusiveDeviceLeaseState::DeviceReserved);
    assert!(!grantor.is_free());

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xd2; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        grantor.grant(&foreign, ExclusiveDeviceLeaseId::from_bytes([0xf3; 16])),
        Err(ExclusiveDeviceLeaseGrantError::IdentityMismatch {
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
        grantor.grant(&wrong_boot, ExclusiveDeviceLeaseId::from_bytes([0xf4; 16])),
        Err(ExclusiveDeviceLeaseGrantError::BootGenerationMismatch {
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
        grantor.grant(&wrong_token, ExclusiveDeviceLeaseId::from_bytes([0xf5; 16])),
        Err(ExclusiveDeviceLeaseGrantError::FencingTokenMismatch {
            presented: CellFencingToken::INITIAL,
            current: current.fencing_token(),
        })
    );
    assert!(!grantor.is_free());

    assert_eq!(
        grantor.grant(&current, ExclusiveDeviceLeaseId::from_bytes([0xf6; 16])),
        Err(ExclusiveDeviceLeaseGrantError::DeviceNotFree { device_id })
    );
    assert!(!grantor.is_free());
}
