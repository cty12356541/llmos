//! Single-Cell `QuotaLease` loss quarantine.
//!
//! `LEASE-LOSS-001`: after the Cell epoch advances, unreconciled face value
//! enters `QUARANTINED` and stays out of `AVAILABLE`. A closed lease is
//! already reconciled and is not re-frozen.
//!
//! One `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch};
use nlos_lease::{QuotaLeaseGrantor, QuotaLeaseLedgerError, QuotaLeaseState};
use nlos_types::{QuotaLeaseId, SchedulerDomainId};

#[test]
fn quota_lease_epoch_advance_quarantines_unreconciled_face() {
    let mut authority =
        CellAuthority::claim(SchedulerDomainId::from_bytes([0xe7; 16])).expect("claim");
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = QuotaLeaseGrantor::open(authority, 100);
    let active_id = QuotaLeaseId::from_bytes([0xe8; 16]);
    let closed_id = QuotaLeaseId::from_bytes([0xe9; 16]);

    grantor
        .grant(&current, active_id, 40)
        .expect("grant active");
    grantor.activate(&current, active_id).expect("activate");
    grantor
        .report_usage(&current, active_id, 15)
        .expect("spend");
    grantor
        .grant(&current, closed_id, 20)
        .expect("grant closed");
    grantor
        .activate(&current, closed_id)
        .expect("activate closed");
    grantor
        .begin_close(&current, closed_id)
        .expect("begin close");
    grantor.ack_close(&current, closed_id).expect("ack close");
    assert_eq!(grantor.available(), 60);

    let next = grantor
        .advance_epoch_and_quarantine()
        .expect("epoch advance quarantines");
    assert_eq!(
        next.epoch(),
        CellEpoch::INITIAL
            .checked_next()
            .expect("epoch 2")
            .checked_next()
            .expect("epoch 3")
    );
    let quarantined = grantor.query(active_id).expect("query active");
    assert_eq!(quarantined.state(), QuotaLeaseState::Quarantined);
    assert_eq!(quarantined.spent(), 15);
    assert_eq!(quarantined.remaining(), 25);
    assert_eq!(quarantined.returned(), 0);
    assert_eq!(
        grantor.query(closed_id).expect("query closed").state(),
        QuotaLeaseState::Closed
    );
    assert_eq!(grantor.available(), 60, "quarantine does not refund");

    assert!(matches!(
        grantor.report_usage(&next, active_id, 15),
        Err(QuotaLeaseLedgerError::NotActive {
            state: QuotaLeaseState::Quarantined
        })
    ));
    assert!(matches!(
        grantor.ack_close(&next, active_id),
        Err(QuotaLeaseLedgerError::NotClosing {
            state: QuotaLeaseState::Quarantined
        })
    ));
    assert_eq!(
        grantor
            .grant(&next, active_id, 40)
            .expect("same id does not deduct again")
            .state(),
        QuotaLeaseState::Quarantined
    );
    assert_eq!(grantor.available(), 60);

    let later = grantor
        .advance_epoch_and_quarantine()
        .expect("second advance");
    assert_eq!(grantor.available(), 60);
    assert_eq!(
        grantor.query(active_id).expect("still quarantined").state(),
        QuotaLeaseState::Quarantined
    );

    let fresh = QuotaLeaseId::from_bytes([0xea; 16]);
    grantor.grant(&later, fresh, 60).expect("pool still usable");
    assert_eq!(grantor.available(), 0);
    assert!(matches!(
        grantor.grant(&later, QuotaLeaseId::from_bytes([0xeb; 16]), 1),
        Err(nlos_lease::QuotaLeaseGrantError::InsufficientAvailable { .. })
    ));
}
