//! Single-Cell `QuotaLease` grant: deduct then issue; stale epoch typed reject.
//!
//! `LEASE-GRANT-001` (in-memory AVAILABLE only): `face_value` is deducted
//! before the lease is issued. No durable second ledger in this slice.
//!
//! Fence admit is [`nlos_cell::CellAuthority::admit`] (one claim per process).
//! One `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_lease::{FenceScope, QuotaLeaseGrantError, QuotaLeaseGrantor, QuotaLeaseState};
use nlos_types::{Generation, QuotaLeaseId, SchedulerDomainId};

#[test]
fn quota_lease_grant_uses_cell_authority_admit_and_deducts_available() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xb1; 16])).expect("claim");

    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    assert_eq!(stale.epoch(), CellEpoch::INITIAL);
    assert_eq!(
        current.epoch(),
        CellEpoch::INITIAL.checked_next().expect("epoch 2")
    );

    let mut grantor = QuotaLeaseGrantor::open(authority, 100);

    assert_eq!(
        grantor.grant(&stale, QuotaLeaseId::from_bytes([0xc2; 16]), 25),
        Err(QuotaLeaseGrantError::StaleEpoch {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        })
    );
    assert_eq!(grantor.available(), 100);

    let lease_id = QuotaLeaseId::from_bytes([0xc1; 16]);
    let grant = grantor
        .grant(&current, lease_id, 40)
        .expect("current fence must grant");

    assert_eq!(grant.lease_id(), lease_id);
    assert_eq!(grant.cell(), current.identity());
    assert_eq!(grant.node_boot_generation(), Generation::INITIAL);
    assert_eq!(grant.face_value(), 40);
    assert_eq!(grant.remaining(), 40);
    assert_eq!(grant.epoch(), current.epoch());
    assert_eq!(grant.fencing_token(), current.fencing_token());
    assert_eq!(grant.fence_scope(), FenceScope::cell(current.identity()));
    assert_eq!(grant.state(), QuotaLeaseState::Issued);
    assert_eq!(grantor.available(), 60);

    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xb2; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        grantor.grant(&foreign, QuotaLeaseId::from_bytes([0xc3; 16]), 10),
        Err(QuotaLeaseGrantError::IdentityMismatch {
            presented: CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xb2; 16])),
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
        grantor.grant(&wrong_boot, QuotaLeaseId::from_bytes([0xc4; 16]), 10),
        Err(QuotaLeaseGrantError::BootGenerationMismatch {
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
        grantor.grant(&wrong_token, QuotaLeaseId::from_bytes([0xc5; 16]), 10),
        Err(QuotaLeaseGrantError::FencingTokenMismatch {
            presented: CellFencingToken::INITIAL,
            current: current.fencing_token(),
        })
    );
    assert_eq!(grantor.available(), 60);

    assert_eq!(
        grantor.grant(&current, QuotaLeaseId::from_bytes([0xc6; 16]), 61),
        Err(QuotaLeaseGrantError::InsufficientAvailable {
            requested: 61,
            available: 60,
        })
    );
    assert_eq!(grantor.available(), 60);
}
