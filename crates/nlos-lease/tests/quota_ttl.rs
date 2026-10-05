//! Single-Cell `QuotaLease` TTL expiry and settle.
//!
//! `LEASE-TTL-001`: a grant may bind a TTL deadline as a logical
//! `LeaseInstant` (this crate reads no wall clock; the caller sweeps with
//! `now`). A lease is expired once `deadline <= now`; the sweep moves
//! `ISSUED`/`ACTIVE`/`CLOSING` leases to `FENCED`. `FENCED` admits no new
//! use; only `settle_fenced` (`FENCED → CLOSED`) returns the unspent
//! remainder, once. `None` deadlines never expire. Epoch loss closes the
//! settle window: un-settled `FENCED` joins `QUARANTINED` (`LEASE-LOSS-001`).
//!
//! In-memory pool only; no durable deadline store in this slice. One
//! `#[test]` so this file claims the process-scoped Cell only once.

use nlos_cell::{CellAuthority, CellEpoch, CellFence};
use nlos_lease::{
    LeaseInstant, QuotaLeaseGrantError, QuotaLeaseGrantor, QuotaLeaseLedgerError, QuotaLeaseState,
};
use nlos_types::{QuotaLeaseId, SchedulerDomainId};

#[test]
fn quota_lease_ttl_deadline_fences_and_settles() {
    let authority = CellAuthority::claim(SchedulerDomainId::from_bytes([0xab; 16])).expect("claim");
    let stale = authority.fence();
    let current = authority.advance_epoch().expect("advance");
    let mut grantor = QuotaLeaseGrantor::open(authority, 1000);

    let first = QuotaLeaseId::from_bytes([0xd1; 16]);
    ttl_binds_and_replays(&mut grantor, &current, first);
    sweep_fences_at_deadline(&mut grantor, &current, first);
    fenced_rejects_new_use_and_settles_once(&mut grantor, &current, &stale, first);
    ttl_free_leases_never_fence(&mut grantor, &current);
    issued_lease_fences_and_settles_full_face(&mut grantor, &current);
    sweep_reports_sorted_ids(&mut grantor, &current);
    epoch_loss_closes_the_settle_window(&mut grantor, &current);
}

fn ttl_binds_and_replays(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    lease_id: QuotaLeaseId,
) {
    let granted = grantor
        .grant(current, lease_id, 40, Some(at(10)))
        .expect("grant with TTL");
    assert_eq!(granted.state(), QuotaLeaseState::Issued);
    assert_eq!(granted.expires_at(), Some(at(10)));
    assert_eq!(grantor.available(), 960);

    let replay = grantor
        .grant(current, lease_id, 40, Some(at(10)))
        .expect("identical grant replays");
    assert_eq!(replay, granted);
    assert_eq!(grantor.available(), 960);

    assert_eq!(
        grantor.grant(current, lease_id, 40, Some(at(20))),
        Err(QuotaLeaseGrantError::ConflictingTtl {
            existing: Some(at(10)),
            requested: Some(at(20)),
        })
    );
    assert_eq!(
        grantor.grant(current, lease_id, 40, None),
        Err(QuotaLeaseGrantError::ConflictingTtl {
            existing: Some(at(10)),
            requested: None,
        })
    );
    assert_eq!(
        grantor.grant(current, lease_id, 50, None),
        Err(QuotaLeaseGrantError::ConflictingFace {
            existing: 40,
            requested: 50,
        })
    );
    assert_eq!(grantor.available(), 960);
}

fn sweep_fences_at_deadline(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    lease_id: QuotaLeaseId,
) {
    grantor.activate(current, lease_id).expect("activate");
    grantor
        .report_usage(current, lease_id, 15)
        .expect("spend before deadline");
    assert_eq!(grantor.query(lease_id).expect("query").spent(), 15);

    assert_eq!(grantor.sweep_expired(at(9)), Vec::<QuotaLeaseId>::new());
    assert_eq!(
        grantor.query(lease_id).expect("query").state(),
        QuotaLeaseState::Active
    );

    assert_eq!(grantor.sweep_expired(at(10)), vec![lease_id]);
    assert_eq!(
        grantor.query(lease_id).expect("query").state(),
        QuotaLeaseState::Fenced
    );
    assert_eq!(grantor.available(), 960, "fencing does not refund");
    assert_eq!(
        grantor.sweep_expired(at(10)),
        Vec::<QuotaLeaseId>::new(),
        "re-sweep is idempotent"
    );
}

fn fenced_rejects_new_use_and_settles_once(
    grantor: &mut QuotaLeaseGrantor,
    current: &CellFence,
    stale: &CellFence,
    lease_id: QuotaLeaseId,
) {
    assert_eq!(
        grantor.report_usage(current, lease_id, 15),
        Err(QuotaLeaseLedgerError::NotActive {
            state: QuotaLeaseState::Fenced,
        })
    );
    assert_eq!(
        grantor.activate(current, lease_id),
        Err(QuotaLeaseLedgerError::ActivateRequiresIssued {
            state: QuotaLeaseState::Fenced,
        })
    );
    assert_eq!(
        grantor.begin_close(current, lease_id),
        Err(QuotaLeaseLedgerError::NotActive {
            state: QuotaLeaseState::Fenced,
        })
    );
    assert_eq!(
        grantor.ack_close(current, lease_id),
        Err(QuotaLeaseLedgerError::NotClosing {
            state: QuotaLeaseState::Fenced,
        })
    );
    assert_eq!(
        grantor.cancel(current, lease_id),
        Err(QuotaLeaseLedgerError::CancelRequiresNeverActive {
            state: QuotaLeaseState::Fenced,
        })
    );
    assert_eq!(
        grantor.settle_fenced(stale, lease_id),
        Err(QuotaLeaseLedgerError::Fence(
            QuotaLeaseGrantError::StaleEpoch {
                presented: CellEpoch::INITIAL,
                current: CellEpoch::INITIAL.checked_next().expect("epoch 2"),
            }
        ))
    );

    let settled = grantor
        .settle_fenced(current, lease_id)
        .expect("settle fenced");
    assert_eq!(settled.state(), QuotaLeaseState::Closed);
    assert_eq!(settled.spent(), 15);
    assert_eq!(settled.remaining(), 0);
    assert_eq!(settled.returned(), 25);
    assert_eq!(grantor.available(), 985);
    assert_eq!(
        grantor
            .settle_fenced(current, lease_id)
            .expect("idempotent settle")
            .state(),
        QuotaLeaseState::Closed
    );
    assert_eq!(grantor.available(), 985, "settle refunds once");

    let active = QuotaLeaseId::from_bytes([0xda; 16]);
    grantor
        .grant(current, active, 5, Some(at(1000)))
        .expect("grant active helper");
    grantor.activate(current, active).expect("activate helper");
    assert_eq!(
        grantor.settle_fenced(current, active),
        Err(QuotaLeaseLedgerError::NotFenced {
            state: QuotaLeaseState::Active,
        })
    );
}

fn ttl_free_leases_never_fence(grantor: &mut QuotaLeaseGrantor, current: &CellFence) {
    let ttl_free = QuotaLeaseId::from_bytes([0xd2; 16]);
    grantor
        .grant(current, ttl_free, 10, None)
        .expect("grant without TTL");
    assert_eq!(grantor.query(ttl_free).expect("query").expires_at(), None);
    assert_eq!(grantor.available(), 970);

    let swept = grantor.sweep_expired(at(2000));
    assert!(!swept.contains(&ttl_free));
    assert_eq!(
        grantor.query(ttl_free).expect("query").state(),
        QuotaLeaseState::Issued
    );
    grantor
        .activate(current, ttl_free)
        .expect("no-TTL lease stays usable");
    grantor
        .begin_close(current, ttl_free)
        .expect("close no-TTL lease");
    grantor
        .ack_close(current, ttl_free)
        .expect("ack no-TTL lease");
    assert_eq!(grantor.available(), 980);
}

fn issued_lease_fences_and_settles_full_face(grantor: &mut QuotaLeaseGrantor, current: &CellFence) {
    let never_active = QuotaLeaseId::from_bytes([0xd3; 16]);
    grantor
        .grant(current, never_active, 20, Some(at(5)))
        .expect("grant issued helper");
    assert_eq!(grantor.sweep_expired(at(5)), vec![never_active]);
    assert_eq!(
        grantor.cancel(current, never_active),
        Err(QuotaLeaseLedgerError::CancelRequiresNeverActive {
            state: QuotaLeaseState::Fenced,
        })
    );

    let settled = grantor
        .settle_fenced(current, never_active)
        .expect("settle never-active");
    assert_eq!(settled.returned(), 20);
    assert_eq!(settled.remaining(), 0);
    assert_eq!(grantor.available(), 980);
}

fn sweep_reports_sorted_ids(grantor: &mut QuotaLeaseGrantor, current: &CellFence) {
    let low = QuotaLeaseId::from_bytes([0x01; 16]);
    let high = QuotaLeaseId::from_bytes([0x02; 16]);
    grantor.grant(current, low, 1, Some(at(30))).expect("low");
    grantor.grant(current, high, 1, Some(at(30))).expect("high");
    assert_eq!(grantor.sweep_expired(at(30)), vec![low, high]);
    assert_eq!(grantor.available(), 978);
}

fn epoch_loss_closes_the_settle_window(grantor: &mut QuotaLeaseGrantor, current: &CellFence) {
    let lost = QuotaLeaseId::from_bytes([0xd4; 16]);
    grantor
        .grant(current, lost, 10, Some(at(50)))
        .expect("grant lost helper");
    grantor.activate(current, lost).expect("activate lost");
    assert_eq!(grantor.sweep_expired(at(50)), vec![lost]);

    let next = grantor
        .advance_epoch_and_quarantine()
        .expect("epoch advance");
    assert_eq!(
        grantor.query(lost).expect("query").state(),
        QuotaLeaseState::Quarantined
    );
    assert_eq!(
        grantor.settle_fenced(&next, lost),
        Err(QuotaLeaseLedgerError::NotFenced {
            state: QuotaLeaseState::Quarantined,
        })
    );
    assert_eq!(grantor.available(), 968, "quarantine does not refund");
}

fn at(tick: u64) -> LeaseInstant {
    LeaseInstant::new(tick)
}
