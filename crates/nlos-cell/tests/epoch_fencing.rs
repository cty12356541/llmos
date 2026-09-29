//! Epoch / fencing admit: stale epoch is a typed fail-closed reject.
//!
//! One `#[test]` so the file claims the process-scoped Cell only once.

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity,
};
use nlos_types::{Generation, SchedulerDomainId};

#[test]
fn epoch_and_fencing_reject_stale_or_mismatched_presentations() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xa1; 16])).expect("claim");

    authority
        .admit(&authority.fence())
        .expect("current fence must admit");

    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    assert_eq!(stale.epoch(), CellEpoch::INITIAL);
    assert_eq!(
        current.epoch(),
        CellEpoch::INITIAL.checked_next().expect("epoch 2")
    );
    assert_eq!(
        authority.admit(&stale),
        Err(CellAdmitError::StaleEpoch {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        })
    );

    let current = authority.fence();
    let wrong_boot = CellFence::present(
        current.identity(),
        Generation::INITIAL.checked_next().expect("boot 2"),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        authority.admit(&wrong_boot),
        Err(CellAdmitError::BootGenerationMismatch {
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
        authority.admit(&wrong_token),
        Err(CellAdmitError::FencingTokenMismatch {
            presented: CellFencingToken::INITIAL,
            current: CellFencingToken::INITIAL.checked_next().expect("token 2"),
        })
    );

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xa2; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        authority.admit(&foreign),
        Err(CellAdmitError::IdentityMismatch {
            presented: CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xa2; 16])),
            current: current.identity(),
        })
    );

    let future_epoch = current.epoch().checked_next().expect("epoch 3");
    let future = CellFence::present(
        current.identity(),
        current.node_boot_generation(),
        future_epoch,
        current.fencing_token(),
    );
    assert_eq!(
        authority.admit(&future),
        Err(CellAdmitError::EpochMismatch {
            presented: future_epoch,
            current: current.epoch(),
        })
    );
}
