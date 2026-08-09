use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    ActivateReservationRequest, ActivationDecision, CreateAccountRequest, CreateQuoteRequest,
    DriverRotationDecision, RegisterDriverRequest, ReservationDecision, ReserveRequest,
    ResourceAuthority, ResourceAuthorityError, RotateDriverRequest,
};
use nlos_types::{CallId, IdempotencyKey, OperationId};
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
            "nlos-resource-{label}-{}-{nonce}-{}",
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

#[test]
fn reserve_is_durable_exactly_replayable_and_conserves_available_credit() {
    let root = Root::new("reserve");
    let request = driver_request(10);
    let (driver, account, quote, reservation) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let driver = authority.register_driver(request).unwrap().record();
        let account = authority.create_account(account_request(10, 100)).unwrap();
        let quote = authority
            .create_quote(quote_request(10, driver, 40))
            .unwrap()
            .record();
        let reserve = reserve_request(10, account, quote);
        let first = authority.reserve(reserve).unwrap();
        assert!(matches!(first, ReservationDecision::Reserved(_)));
        let replay = authority.reserve(reserve).unwrap();
        assert!(matches!(replay, ReservationDecision::Replayed(_)));
        assert_eq!(first.record(), replay.record());
        assert_eq!(
            authority
                .inspect_account(account.account_id)
                .unwrap()
                .available_credit,
            60
        );
        (driver, account, quote, first.record())
    };
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(reopened.register_driver(request).unwrap().record(), driver);
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        60
    );
    assert_eq!(
        reopened
            .inspect_permit_binding(reservation.reservation_id)
            .unwrap(),
        reservation
    );
    assert_eq!(quote.upper_bound, 40);
}

#[test]
fn insufficient_credit_and_idempotency_rebinding_fail_closed() {
    let root = Root::new("conflict");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let d = authority
        .register_driver(driver_request(20))
        .unwrap()
        .record();
    let a = authority.create_account(account_request(20, 20)).unwrap();
    let q = authority
        .create_quote(quote_request(20, d, 30))
        .unwrap()
        .record();
    let r = reserve_request(20, a, q);
    assert!(matches!(
        authority.reserve(r),
        Err(ResourceAuthorityError::InsufficientCredit {
            available: 20,
            required: 30
        })
    ));
    let mut changed = driver_request(20);
    changed.profile_digest = [0xee; 32];
    assert!(matches!(
        authority.register_driver(changed),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));
    assert_eq!(
        authority
            .inspect_account(a.account_id)
            .unwrap()
            .available_credit,
        20
    );
}

#[test]
fn activation_consumes_exact_binding_once_and_replays_receipt() {
    let root = Root::new("activate");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let d = authority
        .register_driver(driver_request(30))
        .unwrap()
        .record();
    let a = authority.create_account(account_request(30, 100)).unwrap();
    let q = authority
        .create_quote(quote_request(30, d, 25))
        .unwrap()
        .record();
    let r = authority
        .reserve(reserve_request(30, a, q))
        .unwrap()
        .record();
    let activate = ActivateReservationRequest {
        reservation_id: r.reservation_id,
        call_id: r.call_id,
        operation_id: r.operation_id,
        driver_id: r.driver_id,
        driver_generation: r.driver_generation,
        driver_fencing_token: r.driver_fencing_token,
        activation_token: r.activation_token,
        activated_at_ms: 3000,
    };
    let first = authority.activate(activate).unwrap();
    assert!(matches!(first, ActivationDecision::Activated(_)));
    let replay = authority.activate(activate).unwrap();
    assert!(matches!(replay, ActivationDecision::Replayed(_)));
    assert_eq!(first.receipt(), replay.receipt());
    assert!(matches!(
        authority.inspect_permit_binding(r.reservation_id),
        Err(ResourceAuthorityError::ReservationAlreadyActive)
    ));
}

#[test]
fn driver_rotation_fences_old_quotes_and_reserved_bindings() {
    let root = Root::new("rotate");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let d = authority
        .register_driver(driver_request(40))
        .unwrap()
        .record();
    let a = authority.create_account(account_request(40, 100)).unwrap();
    let q = authority
        .create_quote(quote_request(40, d, 20))
        .unwrap()
        .record();
    let r = authority
        .reserve(reserve_request(40, a, q))
        .unwrap()
        .record();
    let rotate = RotateDriverRequest {
        driver_id: d.driver_id,
        expected_generation: d.generation,
        expected_fencing_token: d.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([0xd0; 16]),
        rotated_at_ms: 4000,
    };
    let next = authority.rotate_driver(rotate).unwrap();
    assert!(matches!(next, DriverRotationDecision::Rotated(_)));
    assert_eq!(next.record().generation.get(), 2);
    assert!(matches!(
        authority.rotate_driver(rotate),
        Ok(DriverRotationDecision::Replayed(_))
    ));
    assert!(matches!(
        authority.inspect_permit_binding(r.reservation_id),
        Err(ResourceAuthorityError::StaleDriver)
    ));
    assert!(matches!(
        authority.create_quote(quote_request(41, d, 10)),
        Err(ResourceAuthorityError::StaleDriver)
    ));
}

#[test]
fn quote_reservation_and_receipt_identity_are_ddl_protected() {
    let root = Root::new("immutable");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let d = authority
        .register_driver(driver_request(50))
        .unwrap()
        .record();
    let a = authority.create_account(account_request(50, 100)).unwrap();
    let q = authority
        .create_quote(quote_request(50, d, 10))
        .unwrap()
        .record();
    let r = authority
        .reserve(reserve_request(50, a, q))
        .unwrap()
        .record();
    drop(authority);
    let raw = Connection::open(root.path().join("resource-authority.db")).unwrap();
    assert!(
        raw.execute(
            "UPDATE quotes SET upper_bound=11 WHERE quote_id=?1",
            [q.quote_id.as_bytes().as_slice()]
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE reservations SET operation_id=?1 WHERE reservation_id=?2",
            rusqlite::params![
                [0xff_u8; 16].as_slice(),
                r.reservation_id.as_bytes().as_slice()
            ]
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM reservations WHERE reservation_id=?1",
            [r.reservation_id.as_bytes().as_slice()]
        )
        .is_err()
    );
}
