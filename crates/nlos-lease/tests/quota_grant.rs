//! Single-Cell `QuotaLease` grant: deduct then issue; stale epoch typed reject.
//!
//! `LEASE-GRANT-001` (in-memory AVAILABLE only): `face_value` is deducted
//! before the lease is issued. No durable second ledger in this slice.
//!
//! Fences are constructed directly from nlos-cell types; this suite does not
//! claim process-scoped [`nlos_cell::CellAuthority`] (one claim per process).

use nlos_cell::{CellEpoch, CellFence, CellFencingToken, CellIdentity};
use nlos_lease::{FenceScope, QuotaLeaseGrantError, QuotaLeaseGrantor, QuotaLeaseState};
use nlos_types::{Generation, QuotaLeaseId, SchedulerDomainId};

fn cell() -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xb1; 16]))
}

fn current_fence() -> CellFence {
    CellFence::present(
        cell(),
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
    )
}

fn advanced_fence() -> CellFence {
    CellFence::present(
        cell(),
        Generation::INITIAL,
        CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        CellFencingToken::INITIAL.checked_next().expect("token 2"),
    )
}

#[test]
fn grant_deducts_face_value_and_issues_quota_lease_on_current_fence() {
    let fence = current_fence();
    let mut grantor = QuotaLeaseGrantor::open(
        fence.identity(),
        fence.node_boot_generation(),
        fence.epoch(),
        fence.fencing_token(),
        100,
    );

    let lease_id = QuotaLeaseId::from_bytes([0xc1; 16]);
    let grant = grantor
        .grant(&fence, lease_id, 40)
        .expect("current fence must grant");

    assert_eq!(grant.lease_id(), lease_id);
    assert_eq!(grant.cell(), fence.identity());
    assert_eq!(grant.node_boot_generation(), Generation::INITIAL);
    assert_eq!(grant.face_value(), 40);
    assert_eq!(grant.remaining(), 40);
    assert_eq!(grant.epoch(), fence.epoch());
    assert_eq!(grant.fencing_token(), fence.fencing_token());
    assert_eq!(grant.fence_scope(), FenceScope::cell(fence.identity()));
    assert_eq!(grant.state(), QuotaLeaseState::Issued);
    assert_eq!(grantor.available(), 60);
}

#[test]
fn grant_rejects_stale_epoch_without_deducting() {
    let stale = current_fence();
    let current = advanced_fence();
    let mut grantor = QuotaLeaseGrantor::open(
        current.identity(),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
        100,
    );

    assert_eq!(
        grantor.grant(&stale, QuotaLeaseId::from_bytes([0xc2; 16]), 25),
        Err(QuotaLeaseGrantError::StaleEpoch {
            presented: CellEpoch::INITIAL,
            current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        })
    );
    assert_eq!(grantor.available(), 100);
}

#[test]
fn grant_rejects_mismatched_identity_boot_or_token() {
    let current = current_fence();
    let mut grantor = QuotaLeaseGrantor::open(
        current.identity(),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
        50,
    );

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
        CellFencingToken::INITIAL.checked_next().expect("token 2"),
    );
    assert_eq!(
        grantor.grant(&wrong_token, QuotaLeaseId::from_bytes([0xc5; 16]), 10),
        Err(QuotaLeaseGrantError::FencingTokenMismatch {
            presented: CellFencingToken::INITIAL.checked_next().expect("token 2"),
            current: CellFencingToken::INITIAL,
        })
    );
    assert_eq!(grantor.available(), 50);
}

#[test]
fn grant_rejects_insufficient_available_without_partial_issue() {
    let fence = current_fence();
    let mut grantor = QuotaLeaseGrantor::open(
        fence.identity(),
        fence.node_boot_generation(),
        fence.epoch(),
        fence.fencing_token(),
        30,
    );

    assert_eq!(
        grantor.grant(&fence, QuotaLeaseId::from_bytes([0xc6; 16]), 31),
        Err(QuotaLeaseGrantError::InsufficientAvailable {
            requested: 31,
            available: 30,
        })
    );
    assert_eq!(grantor.available(), 30);
}
