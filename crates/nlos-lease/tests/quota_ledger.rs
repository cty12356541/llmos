//! Single-Cell `QuotaLease` local high-water, close, and pre-active cancel.
//!
//! `LEASE-SPEND-001`: usage moves only inside the issued face value.
//! `LEASE-REPORT-001`: the usage high-water is monotonic and idempotent.
//! `LEASE-CLOSE-001`: `ACTIVE → CLOSING` forbids a higher high-water and does
//! not return remainder to AVAILABLE until close ACK.
//! `LEASE-PREACTIVE-001`: `ISSUED` cancel is idempotent and refunds the face
//! value; a lease that has been `ACTIVE` cannot cancel.
//!
//! In-memory pool only. No durable ledger, reconciliation receipt, quarantine,
//! or cross-cell transport. One `#[test]` so this file claims the process-scoped
//! Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence, CellIdentity};
use nlos_lease::{QuotaLeaseGrantError, QuotaLeaseGrantor, QuotaLeaseLedgerError, QuotaLeaseState};
use nlos_types::{QuotaLeaseId, SchedulerDomainId};

#[test]
fn quota_lease_high_water_close_and_preactive_cancel() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xe1; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = QuotaLeaseGrantor::open(authority, 100);
    let lease_id = QuotaLeaseId::from_bytes([0x11; 16]);

    issue_refuses_spend_and_face_change(&mut grantor, &current, lease_id);
    activate_rejects_stale_and_foreign(&mut grantor, &current, &stale, lease_id);
    spend_close_then_cancel_second(&mut grantor, &current, lease_id);
}

fn issue_refuses_spend_and_face_change(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    lease_id: QuotaLeaseId,
) {
    let issued = grantor
        .grant(current, lease_id, 40)
        .expect("issue prepaid lease");
    assert_eq!(issued.state(), QuotaLeaseState::Issued);
    assert_eq!(issued.remaining(), 40);
    assert_eq!(issued.spent(), 0);
    assert_eq!(issued.returned(), 0);
    assert_eq!(grantor.available(), 60);
    assert_eq!(
        grantor.report_usage(current, lease_id, 10),
        Err(QuotaLeaseLedgerError::NotActive {
            state: QuotaLeaseState::Issued,
        })
    );

    let replay = grantor
        .grant(current, lease_id, 40)
        .expect("identical grant replays the committed lease");
    assert_eq!(replay, issued);
    assert_eq!(grantor.available(), 60);
    assert_eq!(
        grantor.grant(current, lease_id, 50),
        Err(QuotaLeaseGrantError::ConflictingFace {
            existing: 40,
            requested: 50,
        })
    );
    assert_eq!(grantor.available(), 60);
}

fn activate_rejects_stale_and_foreign(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    stale: &CellFence,
    lease_id: QuotaLeaseId,
) {
    let active = grantor.activate(current, lease_id).expect("activate");
    assert_eq!(active.state(), QuotaLeaseState::Active);
    assert_eq!(
        grantor
            .activate(current, lease_id)
            .expect("idempotent activate")
            .state(),
        QuotaLeaseState::Active
    );
    assert_eq!(
        grantor.report_usage(stale, lease_id, 10),
        Err(QuotaLeaseLedgerError::Fence(
            QuotaLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );
    let foreign = CellFence::present(
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xe2; 16])),
        current.node_boot_generation(),
        current.epoch(),
        current.fencing_token(),
    );
    assert_eq!(
        grantor.report_usage(&foreign, lease_id, 10),
        Err(QuotaLeaseLedgerError::Fence(
            QuotaLeaseGrantError::IdentityMismatch {
                presented: CellIdentity::from_domain(SchedulerDomainId::from_bytes([0xe2; 16])),
                current: current.identity(),
            }
        ))
    );
}

fn spend_close_then_cancel_second(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    lease_id: QuotaLeaseId,
) {
    let spent = grantor
        .report_usage(current, lease_id, 15)
        .expect("monotonic high-water");
    assert_eq!(spent.spent(), 15);
    assert_eq!(spent.remaining(), 25);
    assert_eq!(
        spent.face_value(),
        spent.remaining() + spent.spent() + spent.returned()
    );
    assert_eq!(grantor.available(), 60);
    assert_eq!(
        grantor
            .report_usage(current, lease_id, 15)
            .expect("idempotent high-water"),
        spent
    );
    assert_eq!(
        grantor.report_usage(current, lease_id, 14),
        Err(QuotaLeaseLedgerError::UsageRegression {
            reported: 14,
            current: 15,
        })
    );
    assert_eq!(
        grantor.report_usage(current, lease_id, 41),
        Err(QuotaLeaseLedgerError::UsageExceedsFace {
            reported: 41,
            face_value: 40,
        })
    );
    assert_eq!(grantor.query(lease_id).expect("query").spent(), 15);
    assert_eq!(
        grantor.cancel(current, lease_id),
        Err(QuotaLeaseLedgerError::CancelRequiresNeverActive {
            state: QuotaLeaseState::Active,
        })
    );

    let closing = grantor.begin_close(current, lease_id).expect("begin close");
    assert_eq!(closing.state(), QuotaLeaseState::Closing);
    assert_eq!(
        grantor.available(),
        60,
        "remainder stays out of AVAILABLE until ACK"
    );
    assert_eq!(
        grantor.report_usage(current, lease_id, 16),
        Err(QuotaLeaseLedgerError::NewReserveForbidden)
    );
    assert_eq!(
        grantor
            .report_usage(current, lease_id, 15)
            .expect("same high-water while closing")
            .state(),
        QuotaLeaseState::Closing
    );
    assert_eq!(
        grantor
            .begin_close(current, lease_id)
            .expect("idempotent close")
            .state(),
        QuotaLeaseState::Closing
    );

    let closed = grantor.ack_close(current, lease_id).expect("close ACK");
    assert_eq!(closed.state(), QuotaLeaseState::Closed);
    assert_eq!(closed.remaining(), 0);
    assert_eq!(closed.spent(), 15);
    assert_eq!(closed.returned(), 25);
    assert_eq!(grantor.available(), 85);
    assert_eq!(
        grantor
            .ack_close(current, lease_id)
            .expect("idempotent ACK")
            .state(),
        QuotaLeaseState::Closed
    );
    assert_eq!(grantor.available(), 85);

    let second = QuotaLeaseId::from_bytes([0x22; 16]);
    grantor.grant(current, second, 10).expect("second issue");
    assert_eq!(grantor.available(), 75);
    let cancelled = grantor.cancel(current, second).expect("pre-active cancel");
    assert_eq!(cancelled.state(), QuotaLeaseState::Cancelled);
    assert_eq!(cancelled.returned(), 10);
    assert_eq!(cancelled.remaining(), 0);
    assert_eq!(grantor.available(), 85);
    assert_eq!(
        grantor
            .cancel(current, second)
            .expect("idempotent cancel")
            .state(),
        QuotaLeaseState::Cancelled
    );
    assert_eq!(grantor.available(), 85);
}
