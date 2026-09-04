//! B-RESOURCE-006 minimal fault matrix: the activation→consume→finalize write
//! chain under kill-window injection must converge to a durable prefix from
//! which `inspect_cost_receipt` returns a byte-equal owner aggregate.
//!
//! Harness follows `finalize_fault_injection.rs` and the Task bridge matrix
//! (`resource_bridge_fault_injection.rs`): `nlos-store-fault` VFS,
//! `FAULT_LOCK` serialization, typed error-chain assertions, raw table
//! counts, and `PRAGMA integrity_check` per scenario.
//!
//! Rows (minimal prefix):
//! - W1 pre-commit IOERR on finalize — typed fail-closed, prefix intact,
//!   redo + `inspect_cost_receipt` converges;
//! - W2 pre-commit ENOSPC on finalize — same;
//! - W3 `PowerLossAfter` at finalize commit boundary — silently dropped
//!   settlement invisible to aggregate, redo byte-equal;
//! - W4 finalize replay storm — N≥3 replays + reopen, one receipt set,
//!   `inspect_cost_receipt` byte-equal every time.

use std::error::Error as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use nlos_resource::{
    ActivateReservationRequest, ConsumeDecision, ConsumeReservationRequest, CreateAccountRequest,
    CreateQuoteRequest, FinalizeDecision, FinalizeReservationRequest, RegisterDriverRequest,
    ReservationState, ResourceAuthority, ResourceAuthorityError, ResourceCostReceipt,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{CallId, IdempotencyKey, OperationId, ReservationId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-resource-cost-receipt-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-resource-cost-receipt-fault-{name}-{}-{sequence}",
                std::process::id()
            )),
        }
    }

    fn db(&self) -> PathBuf {
        self.path.join("resource-authority.db")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn open_shim(root: &Path) -> ResourceAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    ResourceAuthority::open_with_vfs(root, Some(VFS_NAME)).expect("open via fault vfs")
}

fn error_chain(error: &ResourceAuthorityError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn assert_sqlite_error_chain(error: &ResourceAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, ResourceAuthorityError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
    );
}

fn assert_integrity(path: &Path) {
    let connection = Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

fn driver_request() -> RegisterDriverRequest {
    RegisterDriverRequest {
        profile_digest: [0x50; 32],
        idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
        created_at_ms: 1000,
    }
}

fn account_request() -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: 1000,
        idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
        created_at_ms: 1000,
    }
}

fn quote_request(d: nlos_resource::DriverRecord) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: d.driver_id,
        driver_generation: d.generation,
        driver_fencing_token: d.fencing_token,
        operation_proposal_digest: [0x53; 32],
        pricing_version: [0x54; 32],
        upper_bound: 100,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([0x55; 16]),
        created_at_ms: 1000,
    }
}

fn reserve(
    authority: &ResourceAuthority,
    account: nlos_resource::AccountRecord,
    quote: nlos_resource::QuoteRecord,
) -> nlos_resource::ReservationRecord {
    authority
        .reserve(nlos_resource::ReserveRequest {
            account_id: account.account_id,
            quote_id: quote.quote_id,
            call_id: CallId::from_bytes([0x56; 16]),
            operation_id: OperationId::from_bytes([0x57; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x58; 16]),
            reserved_at_ms: 2000,
        })
        .unwrap()
        .record()
}

#[allow(clippy::large_types_passed_by_value)]
fn activate(
    authority: &ResourceAuthority,
    reservation: nlos_resource::ReservationRecord,
) -> nlos_resource::ReservationRecord {
    let activation = match authority
        .activate(ActivateReservationRequest {
            reservation_id: reservation.reservation_id,
            call_id: reservation.call_id,
            operation_id: reservation.operation_id,
            driver_id: reservation.driver_id,
            driver_generation: reservation.driver_generation,
            driver_fencing_token: reservation.driver_fencing_token,
            activation_token: reservation.activation_token,
            activated_at_ms: 3000,
        })
        .unwrap()
    {
        nlos_resource::ActivationDecision::Activated(receipt)
        | nlos_resource::ActivationDecision::Replayed(receipt) => receipt,
    };
    nlos_resource::ReservationRecord {
        activation_receipt_id: Some(activation.receipt_id),
        ..reservation
    }
}

/// Seeds driver + account + quote, reserves, activates, consumes seq 1 usage 37.
fn seed_active_prefix(
    authority: &ResourceAuthority,
) -> (
    nlos_resource::AccountRecord,
    nlos_resource::QuoteRecord,
    nlos_resource::ReservationRecord,
) {
    let driver = authority
        .register_driver(driver_request())
        .unwrap()
        .record();
    let account = authority.create_account(account_request()).unwrap();
    let quote = authority
        .create_quote(quote_request(driver))
        .unwrap()
        .record();
    let reservation = activate(authority, reserve(authority, account, quote));
    assert!(matches!(
        authority.consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: reservation.activation_receipt_id.unwrap(),
            sequence: 1,
            cumulative_usage: 37,
            consumed_at_ms: 4_001,
        }),
        Ok(ConsumeDecision::Recorded(_))
    ));
    (account, quote, reservation)
}

fn finalize_request(reservation: &nlos_resource::ReservationRecord) -> FinalizeReservationRequest {
    FinalizeReservationRequest {
        reservation_id: reservation.reservation_id,
        operation_id: reservation.operation_id,
        activation_receipt_id: reservation.activation_receipt_id.unwrap(),
        effect_closed_proof_digest: [0xcc; 32],
        final_seq: 2,
        final_usage: 37,
        finalized_at_ms: 5000,
    }
}

fn finalize_receipt(decision: FinalizeDecision) -> nlos_resource::FinalizationReceipt {
    match decision {
        FinalizeDecision::Finalized(receipt) | FinalizeDecision::Replayed(receipt) => receipt,
    }
}

fn assert_cost_receipt_not_terminal(authority: &ResourceAuthority, reservation_id: ReservationId) {
    assert!(matches!(
        authority.inspect_cost_receipt(reservation_id),
        Err(ResourceAuthorityError::ReservationNotActive)
    ));
}

fn assert_cost_receipt_aggregate(
    authority: &ResourceAuthority,
    account: nlos_resource::AccountRecord,
    quote: nlos_resource::QuoteRecord,
    reservation: &nlos_resource::ReservationRecord,
    expected: &ResourceCostReceipt,
) {
    let aggregate = authority
        .inspect_cost_receipt(reservation.reservation_id)
        .expect("FINALIZED reservation must yield cost receipt");
    assert_eq!(aggregate, *expected);
    assert_eq!(aggregate.reservation_id, reservation.reservation_id);
    assert_eq!(aggregate.account_id, account.account_id);
    assert_eq!(aggregate.quote_id, quote.quote_id);
    assert_eq!(aggregate.upper_bound, 100);
    assert_eq!(aggregate.consumptions.len(), 1);
    assert_eq!(aggregate.consumptions[0].sequence, 1);
    assert_eq!(aggregate.consumptions[0].cumulative_usage, 37);
    assert_eq!(aggregate.finalization.final_usage, 37);
    assert_eq!(aggregate.finalization.refund_credit, 63);
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
}

fn settle_and_capture_aggregate(
    authority: &ResourceAuthority,
    account: nlos_resource::AccountRecord,
    quote: nlos_resource::QuoteRecord,
    reservation: &nlos_resource::ReservationRecord,
) -> ResourceCostReceipt {
    let _finalized = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(reservation))
            .unwrap(),
    );
    let aggregate = authority
        .inspect_cost_receipt(reservation.reservation_id)
        .expect("settled reservation must aggregate");
    assert_cost_receipt_aggregate(authority, account, quote, reservation, &aggregate);
    aggregate
}

fn assert_no_half_finalize(
    authority: &ResourceAuthority,
    database: &TestRoot,
    reservation: &nlos_resource::ReservationRecord,
) {
    assert_eq!(
        raw_count(&database.db(), "reservation_finalize_receipts"),
        0
    );
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Active
    );
    assert_cost_receipt_not_terminal(authority, reservation.reservation_id);
}

// ---------------------------------------------------------------------------
// W1: pre-commit IOERR on finalize
// ---------------------------------------------------------------------------

/// W1：finalize 结算事务 pre-commit IOERR → typed fail-closed、无半截状态、
/// activation/consume 前缀可聚合拒绝（`ReservationNotActive`）；disarm 后同一
/// finalize 成功且 `inspect_cost_receipt` 逐字节收敛，重启后不变。
#[test]
fn fault_ioerr_on_finalize_cost_receipt_converges_from_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("ioerr-cost-receipt");
    let authority = open_shim(&root.path);
    let (account, quote, reservation) = seed_active_prefix(&authority);

    assert_cost_receipt_not_terminal(&authority, reservation.reservation_id);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_reservation(finalize_request(&reservation))
        .expect_err("finalize must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert_no_half_finalize(&authority, &root, &reservation);

    nlos_store_fault::disarm();
    let aggregate = settle_and_capture_aggregate(&authority, account, quote, &reservation);
    assert_integrity(&root.db());
    drop(authority);

    let reopened = ResourceAuthority::open(&root.path).expect("reopen after recovery");
    assert_cost_receipt_aggregate(&reopened, account, quote, &reservation, &aggregate);
    assert_integrity(&root.db());
}

// ---------------------------------------------------------------------------
// W2: pre-commit ENOSPC on finalize
// ---------------------------------------------------------------------------

/// W2：finalize 结算事务 pre-commit ENOSPC → 同 W1 收敛语义。
#[test]
fn fault_enospc_on_finalize_cost_receipt_converges_from_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("full-cost-receipt");
    let authority = open_shim(&root.path);
    let (account, quote, reservation) = seed_active_prefix(&authority);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_reservation(finalize_request(&reservation))
        .expect_err("finalize must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_no_half_finalize(&authority, &root, &reservation);

    nlos_store_fault::disarm();
    let aggregate = settle_and_capture_aggregate(&authority, account, quote, &reservation);
    assert_integrity(&root.db());
    drop(authority);

    let reopened = ResourceAuthority::open(&root.path).expect("reopen after recovery");
    assert_cost_receipt_aggregate(&reopened, account, quote, &reservation, &aggregate);
}

// ---------------------------------------------------------------------------
// W3: PowerLossAfter at finalize commit boundary
// ---------------------------------------------------------------------------

/// W3：`PowerLossAfter { 0 }` 在 finalize commit 边界静默丢写 → 幻影结算不可
/// 见于 aggregate（`ReservationNotActive`、无 finalize receipt 行）；同一
/// finalize 重做后 `inspect_cost_receipt` 逐字节持久且 receipt id 确定性复用。
#[test]
fn fault_power_loss_at_finalize_commit_cost_receipt_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("power-loss-cost-receipt");
    let authority = open_shim(&root.path);
    let (account, quote, reservation) = seed_active_prefix(&authority);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(&reservation))
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = ResourceAuthority::open(&root.path).expect("reopen after power loss");
    assert_no_half_finalize(&recovered, &root, &reservation);
    assert_integrity(&root.db());

    let aggregate = settle_and_capture_aggregate(&recovered, account, quote, &reservation);
    assert_eq!(
        aggregate.finalization.receipt_id, phantom.receipt_id,
        "deterministic receipt id must be reused after invisible loss"
    );
    drop(recovered);

    let verified = ResourceAuthority::open(&root.path).expect("reopen after redo");
    assert_cost_receipt_aggregate(&verified, account, quote, &reservation, &aggregate);
    assert_integrity(&root.db());
}

// ---------------------------------------------------------------------------
// W4: finalize replay storm + inspect_cost_receipt idempotency
// ---------------------------------------------------------------------------

/// W4：finalize 成功后同一请求连续重放 3 次 + 重开后再重放 1 次 → 每次
/// `Replayed` 且 `inspect_cost_receipt` 逐字节相等；finalize receipt 表恰好
/// 一行，aggregate 不因重放而漂移。
#[test]
fn fault_finalize_replay_storm_inspect_cost_receipt_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("replay-storm-cost-receipt");
    let authority = open_shim(&root.path);
    let (account, quote, reservation) = seed_active_prefix(&authority);
    let request = finalize_request(&reservation);

    let finalized = match authority.finalize_reservation(request).unwrap() {
        FinalizeDecision::Finalized(receipt) => receipt,
        FinalizeDecision::Replayed(_) => panic!("first finalize must create receipt"),
    };
    let aggregate = authority
        .inspect_cost_receipt(reservation.reservation_id)
        .expect("committed reservation must aggregate");
    assert_eq!(aggregate.finalization, finalized);
    assert_eq!(aggregate.finalization.refund_credit, 63);

    for round in 0..3 {
        let replay = finalize_receipt(
            authority
                .finalize_reservation(request)
                .unwrap_or_else(|error| panic!("storm replay {round} must succeed: {error}")),
        );
        assert_eq!(
            replay, finalized,
            "storm replay {round} must match original"
        );
        assert_cost_receipt_aggregate(&authority, account, quote, &reservation, &aggregate);
    }
    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 1);

    drop(authority);
    let reopened = ResourceAuthority::open(&root.path).expect("reopen after storm");
    let replay = finalize_receipt(
        reopened
            .finalize_reservation(request)
            .expect("replay after reopen"),
    );
    assert_eq!(replay, finalized);
    assert_cost_receipt_aggregate(&reopened, account, quote, &reservation, &aggregate);
    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 1);
    assert_integrity(&root.db());
}
