//! B-RESOURCE lifecycle-prefix acceptance evidence.
//!
//! The activation, consumption, and finalization authorities are durable
//! boundaries.  This test deliberately drops and reopens the owner authority
//! after each boundary, then verifies that every immutable receipt and the
//! reservation/account projections still agree before the next transition.
//! It proves a bounded local `SQLite` restart prefix; it does not claim
//! cross-authority `TaskWriteSet` settlement or an endpoint-signed usage proof.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    ActivateReservationRequest, ActivationDecision, ConsumeDecision, ConsumeReservationRequest,
    CreateAccountRequest, CreateQuoteRequest, FinalizeDecision, FinalizeReservationRequest,
    RegisterDriverRequest, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceAuthorityError,
};
use nlos_types::{CallId, IdempotencyKey, OperationId};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before Unix epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-lifecycle-prefix-{}-{nonce}-{}",
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

fn driver_request(seed: u8) -> RegisterDriverRequest {
    RegisterDriverRequest {
        profile_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000,
    }
}

fn account_request(seed: u8, initial_credit: u64) -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
        created_at_ms: 1_000,
    }
}

fn quote_request(seed: u8, driver: nlos_resource::DriverRecord) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: driver.driver_id,
        driver_generation: driver.generation,
        driver_fencing_token: driver.fencing_token,
        operation_proposal_digest: [seed.wrapping_add(3); 32],
        pricing_version: [seed.wrapping_add(4); 32],
        upper_bound: 100,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        created_at_ms: 1_000,
    }
}

fn reserve_request(
    seed: u8,
    account: nlos_resource::AccountRecord,
    quote: nlos_resource::QuoteRecord,
) -> ReserveRequest {
    ReserveRequest {
        account_id: account.account_id,
        quote_id: quote.quote_id,
        call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
        operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
        reserved_at_ms: 2_000,
    }
}

fn activate_request(reservation: &nlos_resource::ReservationRecord) -> ActivateReservationRequest {
    ActivateReservationRequest {
        reservation_id: reservation.reservation_id,
        call_id: reservation.call_id,
        operation_id: reservation.operation_id,
        driver_id: reservation.driver_id,
        driver_generation: reservation.driver_generation,
        driver_fencing_token: reservation.driver_fencing_token,
        activation_token: reservation.activation_token,
        activated_at_ms: 3_000,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn activation_consume_finalize_prefix_replays_after_each_restart() {
    let root = Root::new();
    let (account, reservation, activation) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority
            .register_driver(driver_request(90))
            .unwrap()
            .record();
        let account = authority
            .create_account(account_request(90, 1_000))
            .unwrap();
        let quote = authority
            .create_quote(quote_request(90, driver))
            .unwrap()
            .record();
        let reservation = authority
            .reserve(reserve_request(90, account, quote))
            .unwrap()
            .record();

        let activation = match authority.activate(activate_request(&reservation)).unwrap() {
            ActivationDecision::Activated(receipt) => receipt,
            ActivationDecision::Replayed(_) => panic!("first activation must create a receipt"),
        };
        assert_eq!(
            authority
                .inspect_activation_receipt(reservation.reservation_id)
                .unwrap(),
            activation
        );
        let active = authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap();
        assert_eq!(active.state, ReservationState::Active);
        assert_eq!(active.activation_receipt_id, Some(activation.receipt_id));
        assert_eq!(active.usage_high_water_seq, 0);
        assert_eq!(active.usage_high_water, 0);
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            900
        );
        (account, active, activation)
    };

    // The activation receipt is the durable hand-off into the consume phase.
    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        authority
            .inspect_activation_receipt(reservation.reservation_id)
            .unwrap(),
        activation
    );
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Active
    );
    let consumed = match authority
        .consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: activation.receipt_id,
            sequence: 1,
            cumulative_usage: 37,
            consumed_at_ms: 4_001,
        })
        .unwrap()
    {
        ConsumeDecision::Recorded(receipt) => receipt,
        ConsumeDecision::Replayed(_) => panic!("first consume must create a receipt"),
    };
    assert_eq!(
        authority
            .inspect_consumption_receipt(reservation.reservation_id, 1)
            .unwrap(),
        consumed
    );
    let after_consume = authority
        .inspect_reservation(reservation.reservation_id)
        .unwrap();
    assert_eq!(after_consume.usage_high_water_seq, 1);
    assert_eq!(after_consume.usage_high_water, 37);
    drop(authority);

    // A second restart must preserve the activation binding and the usage
    // high-water before the owner accepts terminal settlement.
    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        authority
            .inspect_activation_receipt(reservation.reservation_id)
            .unwrap(),
        activation
    );
    assert_eq!(
        authority
            .inspect_consumption_receipt(reservation.reservation_id, 1)
            .unwrap(),
        consumed
    );
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900,
        "the upper-bound hold remains until finalize"
    );

    let finalize_request = FinalizeReservationRequest {
        reservation_id: reservation.reservation_id,
        operation_id: reservation.operation_id,
        activation_receipt_id: activation.receipt_id,
        effect_closed_proof_digest: [0xa5; 32],
        final_seq: 2,
        final_usage: 37,
        finalized_at_ms: 5_000,
    };
    let finalized = match authority.finalize_reservation(finalize_request).unwrap() {
        FinalizeDecision::Finalized(receipt) => receipt,
        FinalizeDecision::Replayed(_) => panic!("first finalize must create a receipt"),
    };
    assert_eq!(finalized.high_water_seq, consumed.sequence);
    assert_eq!(finalized.high_water, consumed.cumulative_usage);
    assert_eq!(finalized.final_usage, 37);
    assert_eq!(finalized.refund_credit, 63);
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        963
    );
    assert_eq!(
        authority
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap(),
        finalized
    );
    drop(authority);

    // Final restart: all three immutable owner receipts and the terminal
    // projection remain mutually bound, and replay cannot double-refund.
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(
        reopened
            .inspect_activation_receipt(reservation.reservation_id)
            .unwrap(),
        activation
    );
    assert_eq!(
        reopened
            .inspect_consumption_receipt(reservation.reservation_id, 1)
            .unwrap(),
        consumed
    );
    assert_eq!(
        reopened
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap(),
        finalized
    );
    let terminal = reopened
        .inspect_reservation(reservation.reservation_id)
        .unwrap();
    assert_eq!(terminal.state, ReservationState::Finalized);
    assert_eq!(terminal.activation_receipt_id, Some(activation.receipt_id));
    assert_eq!(terminal.finalize_receipt_id, Some(finalized.receipt_id));
    assert_eq!(terminal.usage_high_water_seq, 1);
    assert_eq!(terminal.usage_high_water, 37);
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        963
    );
    assert!(matches!(
        reopened.finalize_reservation(finalize_request),
        Ok(FinalizeDecision::Replayed(receipt)) if receipt == finalized
    ));
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        963,
        "terminal replay must not credit the refund twice"
    );
    assert!(matches!(
        reopened.consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: activation.receipt_id,
            sequence: 2,
            cumulative_usage: 38,
            consumed_at_ms: 5_001,
        }),
        Err(ResourceAuthorityError::ReservationFinalized)
    ));
}
