//! B-RESOURCE owner aggregate receipt evidence.
//!
//! The aggregate is intentionally read-only: it is built from the durable
//! activation, consumption, and finalization tables and remains byte-equal
//! after an authority restart.  It is the bounded owner-side input for a
//! later `TaskAuthority` nested `resource_and_cost_receipts` integration.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_resource::{
    ActivateReservationRequest, ActivationDecision, ConsumeReservationRequest,
    CreateAccountRequest, CreateQuoteRequest, FinalizeDecision, FinalizeReservationRequest,
    RegisterDriverRequest, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceAuthorityError,
};
use nlos_types::{CallId, IdempotencyKey, OperationId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-cost-receipt-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
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

#[test]
#[allow(clippy::too_many_lines)]
fn cost_receipt_is_owner_derived_and_replays_after_restart() {
    let root = Root::new();
    let (reservation_id, expected) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [0x11; 32],
                idempotency_key: IdempotencyKey::from_bytes([0x12; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes([0x13; 16]),
                created_at_ms: 1_000,
            })
            .unwrap();
        let quote = authority
            .create_quote(CreateQuoteRequest {
                driver_id: driver.driver_id,
                driver_generation: driver.generation,
                driver_fencing_token: driver.fencing_token,
                operation_proposal_digest: [0x14; 32],
                pricing_version: [0x15; 32],
                upper_bound: 100,
                valid_until_ms: 10_000,
                idempotency_key: IdempotencyKey::from_bytes([0x16; 16]),
                created_at_ms: 1_000,
            })
            .unwrap()
            .record();
        let reservation = authority
            .reserve(ReserveRequest {
                account_id: account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([0x17; 16]),
                operation_id: OperationId::from_bytes([0x18; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0x19; 16]),
                reserved_at_ms: 2_000,
            })
            .unwrap()
            .record();
        let activation = match authority
            .activate(ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 3_000,
            })
            .unwrap()
        {
            ActivationDecision::Activated(receipt) => receipt,
            ActivationDecision::Replayed(_) => panic!("first activation must create receipt"),
        };
        authority
            .consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence: 1,
                cumulative_usage: 37,
                consumed_at_ms: 4_000,
            })
            .unwrap();
        let finalized = match authority
            .finalize_reservation(FinalizeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                effect_closed_proof_digest: [0x1a; 32],
                final_seq: 2,
                final_usage: 37,
                finalized_at_ms: 5_000,
            })
            .unwrap()
        {
            FinalizeDecision::Finalized(receipt) => receipt,
            FinalizeDecision::Replayed(_) => panic!("first finalize must create receipt"),
        };
        let aggregate = authority
            .inspect_cost_receipt(reservation.reservation_id)
            .unwrap();
        assert_eq!(aggregate.reservation_id, reservation.reservation_id);
        assert_eq!(aggregate.account_id, account.account_id);
        assert_eq!(aggregate.quote_id, quote.quote_id);
        assert_eq!(aggregate.call_id, reservation.call_id);
        assert_eq!(aggregate.operation_id, reservation.operation_id);
        assert_eq!(aggregate.upper_bound, 100);
        assert_eq!(aggregate.activation, activation);
        assert_eq!(aggregate.consumptions.len(), 1);
        assert_eq!(aggregate.consumptions[0].sequence, 1);
        assert_eq!(aggregate.consumptions[0].cumulative_usage, 37);
        assert_eq!(aggregate.finalization, finalized);
        assert_eq!(finalized.refund_credit, 63);
        assert_eq!(
            authority
                .inspect_reservation(reservation.reservation_id)
                .unwrap()
                .state,
            ReservationState::Finalized
        );
        (reservation.reservation_id, aggregate)
    };

    let reopened = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        reopened.inspect_cost_receipt(reservation_id).unwrap(),
        expected,
        "owner aggregate must replay exactly after restart"
    );
    assert!(matches!(
        reopened.inspect_cost_receipt(expected.reservation_id),
        Ok(receipt) if receipt.finalization.refund_credit == 63
    ));
}

#[test]
fn cost_receipt_requires_terminal_owner_state() {
    let root = Root::new();
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let driver = authority
        .register_driver(RegisterDriverRequest {
            profile_digest: [0x21; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let account = authority
        .create_account(CreateAccountRequest {
            initial_credit: 10,
            idempotency_key: IdempotencyKey::from_bytes([0x23; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    let quote = authority
        .create_quote(CreateQuoteRequest {
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            operation_proposal_digest: [0x24; 32],
            pricing_version: [0x25; 32],
            upper_bound: 5,
            valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([0x26; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let reservation = authority
        .reserve(ReserveRequest {
            account_id: account.account_id,
            quote_id: quote.quote_id,
            call_id: CallId::from_bytes([0x27; 16]),
            operation_id: OperationId::from_bytes([0x28; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x29; 16]),
            reserved_at_ms: 2_000,
        })
        .unwrap()
        .record();
    assert!(matches!(
        authority.inspect_cost_receipt(reservation.reservation_id),
        Err(ResourceAuthorityError::ReservationNotActive)
    ));
}
