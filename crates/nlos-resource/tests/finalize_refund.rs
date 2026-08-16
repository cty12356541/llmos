//! B-RESOURCE-005 acceptance tests: the double-entry Reservation
//! finalize/refund settlement overlay (schema v5).
//!
//! An ACTIVE Reservation whose external effect is proven closed settles in
//! one transaction: an immutable `FinalizationReceipt` (with
//! `effect_closed_proof_digest`, final sequence/usage and the derived
//! `refund_credit = upper_bound - final_usage`) plus the Reservation
//! `FINALIZED` overlay plus the account refund. Late consume/quarantine are
//! rejected; exact replay returns the original receipt; different bytes
//! fail closed; the overlay survives restart and the v4→v5 migration
//! re-applies idempotently (partial schema fails closed).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    ActivateReservationRequest, ActivationDecision, ConsumeDecision, ConsumeReservationRequest,
    CreateAccountRequest, CreateQuoteRequest, FinalizationReceipt, FinalizeDecision,
    FinalizeReservationRequest, QuarantineDecision, QuarantineReservationRequest,
    RegisterDriverRequest, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceAuthorityError,
};
use nlos_types::{CallId, IdempotencyKey, OperationId, ReceiptId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Root(PathBuf);
impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-finalize-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn driver_request(seed: u8) -> RegisterDriverRequest {
    RegisterDriverRequest {
        profile_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1000,
    }
}
fn account_request(seed: u8, credit: u64) -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: credit,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
        created_at_ms: 1000,
    }
}
fn quote_request(seed: u8, d: nlos_resource::DriverRecord, upper: u64) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: d.driver_id,
        driver_generation: d.generation,
        driver_fencing_token: d.fencing_token,
        operation_proposal_digest: [seed.wrapping_add(3); 32],
        pricing_version: [seed.wrapping_add(4); 32],
        upper_bound: upper,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        created_at_ms: 1000,
    }
}
fn reserve_request(
    seed: u8,
    a: nlos_resource::AccountRecord,
    q: nlos_resource::QuoteRecord,
) -> ReserveRequest {
    ReserveRequest {
        account_id: a.account_id,
        quote_id: q.quote_id,
        call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
        operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
        reserved_at_ms: 2000,
    }
}

/// Seeds driver + account + quote and returns the ACTIVE reservation
/// (activation already applied) ready for consume/finalize.
fn seed_active(
    authority: &ResourceAuthority,
    seed: u8,
    credit: u64,
    upper_bound: u64,
) -> (
    nlos_resource::DriverRecord,
    nlos_resource::AccountRecord,
    nlos_resource::ReservationRecord,
) {
    let driver = authority
        .register_driver(driver_request(seed))
        .unwrap()
        .record();
    let account = authority
        .create_account(account_request(seed, credit))
        .unwrap();
    let quote = authority
        .create_quote(quote_request(seed, driver, upper_bound))
        .unwrap()
        .record();
    let reservation = authority
        .reserve(reserve_request(seed, account, quote))
        .unwrap()
        .record();
    let activate = ActivateReservationRequest {
        reservation_id: reservation.reservation_id,
        call_id: reservation.call_id,
        operation_id: reservation.operation_id,
        driver_id: reservation.driver_id,
        driver_generation: reservation.driver_generation,
        driver_fencing_token: reservation.driver_fencing_token,
        activation_token: reservation.activation_token,
        activated_at_ms: 3000,
    };
    assert!(matches!(
        authority.activate(activate),
        Ok(ActivationDecision::Activated(_))
    ));
    let activation = authority
        .inspect_activation_receipt(reservation.reservation_id)
        .unwrap();
    (
        driver,
        account,
        nlos_resource::ReservationRecord {
            activation_receipt_id: Some(activation.receipt_id),
            ..reservation
        },
    )
}

fn finalize_request(
    reservation: &nlos_resource::ReservationRecord,
    final_seq: u64,
    final_usage: u64,
    proof_seed: u8,
) -> FinalizeReservationRequest {
    FinalizeReservationRequest {
        reservation_id: reservation.reservation_id,
        operation_id: reservation.operation_id,
        activation_receipt_id: reservation
            .activation_receipt_id
            .expect("activated reservation"),
        effect_closed_proof_digest: [proof_seed; 32],
        final_seq,
        final_usage,
        finalized_at_ms: 5000,
    }
}

fn finalize_receipt(decision: FinalizeDecision) -> FinalizationReceipt {
    match decision {
        FinalizeDecision::Finalized(receipt) | FinalizeDecision::Replayed(receipt) => receipt,
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full double-entry settle + immutability lifecycle.
fn finalize_settles_double_entry_refund_and_is_immutable() {
    let root = Root::new("settle");
    let (account, reservation) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let (_, account, reservation) = seed_active(&authority, 40, 1000, 100);
        // Observe usage 40 at seq 1.
        assert!(matches!(
            authority.consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: reservation.activation_receipt_id.unwrap(),
                sequence: 1,
                cumulative_usage: 40,
                consumed_at_ms: 4_001,
            }),
            Ok(ConsumeDecision::Recorded(_))
        ));
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            900,
            "the full upper bound stays reserved until finalize"
        );

        // Effect-closed settle: final usage 40 (== high water), seq 2.
        let settled = finalize_receipt(
            authority
                .finalize_reservation(finalize_request(&reservation, 2, 40, 0xaa))
                .unwrap(),
        );
        assert_eq!(settled.refund_credit, 60);
        assert_eq!(settled.high_water_seq, 1);
        assert_eq!(settled.final_seq, 2);
        assert_eq!(settled.high_water, 40);
        assert_eq!(settled.final_usage, 40);
        assert_eq!(settled.effect_closed_proof_digest, [0xaa; 32]);
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            960,
            "refund (100 - 40) is credited in the same transaction"
        );

        // FINALIZED overlay + immutable readback.
        let stored = authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap();
        assert_eq!(stored.state, ReservationState::Finalized);
        assert_eq!(stored.finalize_receipt_id, Some(settled.receipt_id));
        assert_eq!(stored.finalized_at_ms, Some(5000));
        assert_eq!(
            authority
                .inspect_finalize_receipt(reservation.reservation_id)
                .unwrap(),
            settled
        );

        // Exact replay returns the original receipt; different bytes fail.
        assert!(matches!(
            authority.finalize_reservation(finalize_request(&reservation, 2, 40, 0xaa)),
            Ok(FinalizeDecision::Replayed(replayed)) if replayed == settled
        ));
        assert!(matches!(
            authority.finalize_reservation(finalize_request(&reservation, 2, 41, 0xaa)),
            Err(ResourceAuthorityError::IdempotencyConflict)
        ));

        // Late consume / quarantine are rejected after settlement.
        assert!(matches!(
            authority.consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: reservation.activation_receipt_id.unwrap(),
                sequence: 2,
                cumulative_usage: 41,
                consumed_at_ms: 5_001,
            }),
            Err(ResourceAuthorityError::ReservationFinalized)
        ));
        assert!(matches!(
            authority.quarantine(QuarantineReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: reservation.activation_receipt_id.unwrap(),
                reason_digest: [0xbb; 32],
                quarantined_at_ms: 5_002,
            }),
            Err(ResourceAuthorityError::ReservationFinalized)
        ));
        (account, reservation)
    };

    // The overlay and receipt survive a full restart readback.
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    let stored = reopened
        .inspect_reservation(reservation.reservation_id)
        .unwrap();
    assert_eq!(stored.state, ReservationState::Finalized);
    assert_eq!(
        reopened
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap()
            .refund_credit,
        60
    );
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );

    // Immutable triggers reject UPDATE/DELETE of the finalize receipt.
    let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
    let receipt_id = stored.finalize_receipt_id.unwrap();
    assert!(
        raw.execute(
            "UPDATE reservation_finalize_receipts SET refund_credit = 0 WHERE receipt_id = ?1",
            rusqlite::params![receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM reservation_finalize_receipts WHERE receipt_id = ?1",
            rusqlite::params![receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE reservations SET finalize_receipt_id = NULL WHERE reservation_id = ?1",
            rusqlite::params![reservation.reservation_id.as_bytes().as_slice()],
        )
        .is_err(),
        "the finalize binding must be immutable once set"
    );
}

#[test]
fn finalize_no_effect_refunds_full_upper_bound() {
    let root = Root::new("no-effect");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let (_, account, reservation) = seed_active(&authority, 50, 1000, 100);
    let settled = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(&reservation, 1, 0, 0xcc))
            .unwrap(),
    );
    assert_eq!(settled.high_water, 0);
    assert_eq!(settled.final_usage, 0);
    assert_eq!(
        settled.refund_credit, 100,
        "no-effect refunds the full hold"
    );
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        1000,
        "double-entry: available credit returns to the pre-reserve balance"
    );
}

#[test]
fn finalize_fails_closed_on_invalid_inputs() {
    let root = Root::new("fail-closed");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let (_, _, reservation) = seed_active(&authority, 60, 1000, 100);
    authority
        .consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: reservation.activation_receipt_id.unwrap(),
            sequence: 1,
            cumulative_usage: 30,
            consumed_at_ms: 4_001,
        })
        .unwrap();

    // Usage above the reserved upper bound.
    assert!(matches!(
        authority.finalize_reservation(finalize_request(&reservation, 2, 101, 0x01)),
        Err(ResourceAuthorityError::UsageExceedsUpperBound { .. })
    ));
    // Usage below the observed high-water.
    assert!(matches!(
        authority.finalize_reservation(finalize_request(&reservation, 2, 29, 0x02)),
        Err(ResourceAuthorityError::UsageNotMonotonic { .. })
    ));
    // Final sequence below the observed high-water sequence (1).
    assert!(matches!(
        authority.finalize_reservation(finalize_request(&reservation, 0, 30, 0x03)),
        Err(ResourceAuthorityError::FinalizeSequenceConflict)
    ));
    // Timestamp before activation.
    let mut early = finalize_request(&reservation, 2, 30, 0x04);
    early.finalized_at_ms = 2999;
    assert!(matches!(
        authority.finalize_reservation(early),
        Err(ResourceAuthorityError::InvalidFinalizeTimestamp)
    ));
    // Wrong operation / activation binding.
    let mut wrong_operation = finalize_request(&reservation, 2, 30, 0x05);
    wrong_operation.operation_id = OperationId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.finalize_reservation(wrong_operation),
        Err(ResourceAuthorityError::ReservationBindingMismatch)
    ));
    let mut wrong_activation = finalize_request(&reservation, 2, 30, 0x06);
    wrong_activation.activation_receipt_id = ReceiptId::from_bytes([0xdd; 16]);
    assert!(matches!(
        authority.finalize_reservation(wrong_activation),
        Err(ResourceAuthorityError::ReservationBindingMismatch)
    ));
    // Unknown reservation.
    assert!(matches!(
        authority.finalize_reservation(FinalizeReservationRequest {
            reservation_id: nlos_types::ReservationId::from_bytes([0xab; 16]),
            ..finalize_request(&reservation, 2, 30, 0x07)
        }),
        Err(ResourceAuthorityError::ReservationNotFound)
    ));
}

#[test]
fn finalize_rejects_reserved_reservations() {
    let root = Root::new("state-guards");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let driver = authority
        .register_driver(driver_request(70))
        .unwrap()
        .record();
    let account = authority.create_account(account_request(70, 1000)).unwrap();
    let quote = authority
        .create_quote(quote_request(70, driver, 100))
        .unwrap()
        .record();
    let reserved = authority
        .reserve(reserve_request(70, account, quote))
        .unwrap()
        .record();

    // A RESERVED (not activated) reservation cannot be finalized.
    assert!(matches!(
        authority.finalize_reservation(FinalizeReservationRequest {
            reservation_id: reserved.reservation_id,
            operation_id: reserved.operation_id,
            activation_receipt_id: ReceiptId::from_bytes([0; 16]),
            effect_closed_proof_digest: [0x11; 32],
            final_seq: 1,
            final_usage: 0,
            finalized_at_ms: 5000,
        }),
        Err(ResourceAuthorityError::ReservationNotActive)
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full quarantine-reconciliation lifecycle.
fn quarantined_reservation_reconciles_with_effect_closed_proof() {
    let root = Root::new("reconcile");
    let (account, reservation, quarantine_receipt) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let (_, account, reservation) = seed_active(&authority, 71, 1000, 100);
        authority
            .consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: reservation.activation_receipt_id.unwrap(),
                sequence: 1,
                cumulative_usage: 30,
                consumed_at_ms: 4_001,
            })
            .unwrap();
        // No effect-closed proof yet -> conservative freeze at high-water 30.
        let quarantine = match authority
            .quarantine(QuarantineReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: reservation.activation_receipt_id.unwrap(),
                reason_digest: [0x22; 32],
                quarantined_at_ms: 4_000,
            })
            .unwrap()
        {
            QuarantineDecision::Quarantined(receipt) => receipt,
            QuarantineDecision::Replayed(_) => panic!("expected Quarantined"),
        };
        assert_eq!(
            authority
                .inspect_reservation(reservation.reservation_id)
                .unwrap()
                .state,
            ReservationState::Quarantined
        );
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            900,
            "the full upper bound stays reserved while frozen"
        );

        // Reconciliation: the effect-closed proof arrives later; the frozen
        // high-water is the baseline and the hold settles with a refund.
        let settled = finalize_receipt(
            authority
                .finalize_reservation(finalize_request(&reservation, 2, 30, 0x33))
                .unwrap(),
        );
        assert_eq!(
            settled.high_water, 30,
            "frozen quarantine high-water is the baseline"
        );
        assert_eq!(settled.final_usage, 30);
        assert_eq!(settled.refund_credit, 70);
        let stored = authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap();
        assert_eq!(stored.state, ReservationState::Finalized);
        assert_eq!(
            stored.quarantine_receipt_id, None,
            "the QUARANTINED overlay is lifted by the settlement"
        );
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            970,
            "refund (100 - 30) is credited in the same transaction"
        );
        assert_eq!(
            authority
                .inspect_finalize_receipt(reservation.reservation_id)
                .unwrap(),
            settled
        );
        // The immutable quarantine receipt row stays as durable evidence,
        // even though the overlay moved on.
        assert!(matches!(
            authority.inspect_quarantine_receipt(reservation.reservation_id),
            Err(ResourceAuthorityError::CorruptRecord(_))
        ));
        // Exact replay returns the original settle receipt.
        assert!(matches!(
            authority.finalize_reservation(finalize_request(&reservation, 2, 30, 0x33)),
            Ok(FinalizeDecision::Replayed(replayed)) if replayed == settled
        ));
        (account, reservation, quarantine)
    };

    let reopened = ResourceAuthority::open(root.path()).unwrap();
    let stored = reopened
        .inspect_reservation(reservation.reservation_id)
        .unwrap();
    assert_eq!(stored.state, ReservationState::Finalized);
    assert_eq!(stored.quarantine_receipt_id, None);
    assert_eq!(
        reopened
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap()
            .refund_credit,
        70
    );
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        970
    );
    // The quarantine receipt row remains durable for audit after restart.
    let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
    let count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM reservation_quarantine_receipts WHERE receipt_id = ?1",
            rusqlite::params![quarantine_receipt.receipt_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        count, 1,
        "the immutable quarantine receipt survives reconciliation"
    );
}

#[test]
fn finalize_v5_migration_reapplies_idempotently_and_partial_schema_fails_closed() {
    let root = Root::new("migration-v5");
    // Build a v5 database with one ACTIVE reservation, then strip the v5
    // artifacts back to a v4-shaped schema (legacy rows keep no overlay).
    let reservation_id = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let (_, _, reservation) = seed_active(&authority, 80, 1000, 100);
        reservation.reservation_id
    };
    {
        let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(
            "DROP TRIGGER reservation_finalize_binding_insert;
             DROP TRIGGER reservation_finalize_binding_update;
             DROP TRIGGER reservation_finalize_receipts_immutable_update;
             DROP TRIGGER reservation_finalize_receipts_immutable_delete;
             DROP TRIGGER reservation_finalize_binding_immutable;
             DROP TABLE reservation_finalize_receipts;
             DROP INDEX reservations_finalize_receipt_unique;
             ALTER TABLE reservations DROP COLUMN finalize_receipt_id;
             ALTER TABLE reservations DROP COLUMN finalized_at_ms;
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }

    // Reopen re-applies v5 idempotently; the legacy ACTIVE row is untouched
    // (no invented overlay) and remains finalizable.
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let legacy = authority.inspect_reservation(reservation_id).unwrap();
    assert_eq!(legacy.state, ReservationState::Active);
    assert_eq!(legacy.finalize_receipt_id, None);
    let settled = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(&legacy, 1, 0, 0x44))
            .unwrap(),
    );
    assert_eq!(settled.refund_credit, 100);

    // A partial v5 schema (columns present, receipt table missing) fails
    // closed instead of silently "upgrading".
    let root2 = Root::new("migration-v5-partial");
    {
        let authority = ResourceAuthority::open(root2.path()).unwrap();
        seed_active(&authority, 81, 1000, 100);
    }
    {
        let raw = Connection::open(root2.path().join("resource-authority.db")).unwrap();
        raw.execute_batch(
            "DROP TRIGGER reservation_finalize_binding_insert;
             DROP TRIGGER reservation_finalize_binding_update;
             DROP TRIGGER reservation_finalize_receipts_immutable_update;
             DROP TRIGGER reservation_finalize_receipts_immutable_delete;
             DROP TRIGGER reservation_finalize_binding_immutable;
             DROP TABLE reservation_finalize_receipts;
             PRAGMA user_version = 4;",
        )
        .unwrap();
    }
    assert!(matches!(
        ResourceAuthority::open(root2.path()),
        Err(ResourceAuthorityError::CorruptRecord(
            "partial resource finalize schema"
        ))
    ));
}
