//! B-RESOURCE-005 fault-injection tests: the schema-v5 finalize/refund
//! settlement table group under the PoC-0003-aligned F1-F4 fault matrix,
//! mirroring the `nlos-task` fault matrices (`takeover_fault_injection.rs`,
//! `reconcile_fault_injection.rs`). The harness reuses the `nlos-store-fault`
//! VFS: kill-9 child processes synchronized through piped `READY` markers
//! (never sleeps), `FAULT_LOCK` process-wide serialization,
//! `wal_commit_frames` tail truncation, typed error-chain assertions, raw
//! table-level counts, and a `PRAGMA integrity_check` re-verification at the
//! end of every scenario.
//!
//! The write paths under test are the single finalize settlement transaction
//! (immutable `FinalizationReceipt` INSERT + FINALIZED overlay UPDATE + the
//! account refund UPDATE, all in one `BEGIN IMMEDIATE`), plus the overlay
//! binding and the account double-entry refund. `nlos-resource` gains the
//! fault-VFS `open_with_vfs` plumbing; the authority code itself is unchanged.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_resource::{
    ActivateReservationRequest, ConsumeDecision, ConsumeReservationRequest, CreateAccountRequest,
    CreateQuoteRequest, FinalizationReceipt, FinalizeDecision, FinalizeReservationRequest,
    RegisterDriverRequest, ReservationState, ResourceAuthority, ResourceAuthorityError,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{CallId, IdempotencyKey, OperationId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-resource-finalize-fault";

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
                "nlos-resource-finalize-fault-{name}-{}-{sequence}",
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

/// Full `Display` chain of a `ResourceAuthorityError`, top cause last.
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
        profile_digest: [0x40; 32],
        idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
        created_at_ms: 1000,
    }
}

fn account_request() -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: 1000,
        idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
        created_at_ms: 1000,
    }
}

fn quote_request(d: nlos_resource::DriverRecord) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: d.driver_id,
        driver_generation: d.generation,
        driver_fencing_token: d.fencing_token,
        operation_proposal_digest: [0x43; 32],
        pricing_version: [0x44; 32],
        upper_bound: 100,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([0x45; 16]),
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
            call_id: CallId::from_bytes([0x46; 16]),
            operation_id: OperationId::from_bytes([0x47; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x48; 16]),
            reserved_at_ms: 2000,
        })
        .unwrap()
        .record()
}

#[allow(clippy::large_types_passed_by_value)] // Keep fixture call sites compact and readable.
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

/// Seeds driver + account + quote, reserves, activates and observes usage 40
/// at seq 1: the ACTIVE finalize precondition.
fn seed_active(
    authority: &ResourceAuthority,
) -> (
    nlos_resource::AccountRecord,
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
            cumulative_usage: 40,
            consumed_at_ms: 4_001,
        }),
        Ok(ConsumeDecision::Recorded(_))
    ));
    (account, reservation)
}

fn finalize_request(reservation: &nlos_resource::ReservationRecord) -> FinalizeReservationRequest {
    FinalizeReservationRequest {
        reservation_id: reservation.reservation_id,
        operation_id: reservation.operation_id,
        activation_receipt_id: reservation.activation_receipt_id.unwrap(),
        effect_closed_proof_digest: [0xaa; 32],
        final_seq: 2,
        final_usage: 40,
        finalized_at_ms: 5000,
    }
}

fn finalize_receipt(decision: FinalizeDecision) -> FinalizationReceipt {
    match decision {
        FinalizeDecision::Finalized(receipt) | FinalizeDecision::Replayed(receipt) => receipt,
    }
}

/// Runs the finalize settlement and asserts the double-entry outcome:
/// receipt refund 60, reservation FINALIZED, account 900 -> 960.
fn settle(authority: &ResourceAuthority, reservation: &nlos_resource::ReservationRecord) {
    let settled = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(reservation))
            .unwrap(),
    );
    assert_eq!(settled.refund_credit, 60);
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
}

fn assert_no_half_finalize(
    authority: &ResourceAuthority,
    database: &TestRoot,
    account: nlos_resource::AccountRecord,
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
        ReservationState::Active,
        "failed finalize must not move the reservation"
    );
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900,
        "failed finalize must not refund"
    );
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_RESOURCE_FINALIZE_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_RESOURCE_FINALIZE_CRASH_CHILD_ROOT", root)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

fn await_marker(child: &mut Child) {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let mut marker = None;
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.starts_with("READY") => {
                    marker = Some(line);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(marker.ok_or_else(|| "child exited without READY".to_string()));
    });
    match receiver.recv_timeout(Duration::from_mins(1)) {
        Ok(Ok(_)) => {}
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

fn wal_commit_frames(wal: &[u8]) -> (usize, Vec<usize>) {
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    let commits: Vec<usize> = (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 8..start + 12].try_into().expect("commit field")) != 0
        })
        .collect();
    assert!(
        commits.len() >= 2,
        "fixture must contain several committed transactions"
    );
    (frame_size, commits)
}

fn truncate_wal_inside_last_commit(db: &Path) {
    let wal_path = sibling_path(db, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(sibling_path(db, "-shm")).expect("remove stale shm");
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

// ---------------------------------------------------------------------------
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// Matrix row 1 fixture: the ACTIVE prefix is committed; a writer transaction
/// then dirties the finalize table group (phantom finalize receipt with the
/// real reservation binding, phantom FINALIZED overlay, phantom refund on the
/// account) and dies before commit.
fn child_mid_finalize_tx(root: &Path) -> ! {
    let authority = ResourceAuthority::open(root).expect("open");
    let (_, reservation) = seed_active(&authority);
    let raw = Connection::open(root.join("resource-authority.db")).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    let reservation_id = reservation.reservation_id.as_bytes();
    let operation_id = reservation.operation_id.as_bytes();
    let activation = reservation
        .activation_receipt_id
        .expect("activated reservation");
    let activation_id = activation.as_bytes();
    raw.execute(
        "INSERT INTO reservation_finalize_receipts (
            receipt_id, reservation_id, operation_id, activation_receipt_id,
            effect_closed_proof_digest, high_water_seq, final_seq,
            high_water, final_usage, refund_credit, finalized_at_ms
         ) VALUES (X'99999999999999999999999999999999', ?1, ?2, ?3,
                   X'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
                   1, 2, 40, 40, 60, 9000)",
        rusqlite::params![reservation_id, operation_id, activation_id],
    )
    .expect("mid-tx phantom finalize receipt");
    raw.execute(
        "UPDATE reservations SET finalize_receipt_id=X'99999999999999999999999999999999',
              finalized_at_ms=9000 WHERE reservation_id=?1",
        rusqlite::params![reservation_id],
    )
    .expect("mid-tx phantom finalize overlay");
    raw.execute(
        "UPDATE resource_accounts SET available_credit=available_credit+1000 WHERE account_id=?1",
        rusqlite::params![
            authority
                .inspect_reservation(reservation.reservation_id)
                .unwrap()
                .account_id
                .as_bytes()
                .as_slice()
        ],
    )
    .expect("mid-tx phantom refund");
    announce("READY");
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// Matrix row 2 fixture: the complete committed finalize settlement
/// (receipt + FINALIZED overlay + account refund) before the kill.
fn child_finalize_commit_complete(root: &Path) -> ! {
    let authority = ResourceAuthority::open(root).expect("open");
    let (_, reservation) = seed_active(&authority);
    settle(&authority, &reservation);
    announce("READY");
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Torn-tail fixture: the finalize settlement is the last committed
/// transaction.
fn child_torn_wal_finalize(root: &Path) -> ! {
    let authority = ResourceAuthority::open(root).expect("open");
    let (_, reservation) = seed_active(&authority);
    settle(&authority, &reservation);
    announce("READY");
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_RESOURCE_FINALIZE_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_RESOURCE_FINALIZE_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "mid-finalize-tx" => child_mid_finalize_tx(&root),
        "finalize-commit-complete" => child_finalize_commit_complete(&root),
        "torn-wal-finalize" => child_torn_wal_finalize(&root),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 矩阵行 1: kill-9 mid-transaction on the finalize table group
// ---------------------------------------------------------------------------

/// kill-9 中断 finalize 表组写事务：子进程在 `BEGIN IMMEDIATE` 未提交（幻影
/// finalize receipt + 幻影 FINALIZED overlay + 幻影账户退款）时被强杀；重开
/// 后中断事务完全回滚——无 finalize receipt 行、reservation 保持 `ACTIVE`、
/// 账户保持 900；随后同一 finalize 重做成功且确定性 receipt id 一致、退款
/// 960 持久。
#[test]
fn fault_kill9_mid_finalize_tx_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("kill9-mid-finalize-tx");
    let mut child = spawn_child("mid-finalize-tx", &root.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 0);
    let authority = ResourceAuthority::open(&root.path).expect("reopen after kill");
    let account = authority.create_account(account_request()).unwrap();
    // Re-derive the reservation through the idempotent replay path.
    let driver = authority
        .register_driver(driver_request())
        .unwrap()
        .record();
    let quote = authority
        .create_quote(quote_request(driver))
        .unwrap()
        .record();
    let reservation = match authority.reserve(nlos_resource::ReserveRequest {
        account_id: account.account_id,
        quote_id: quote.quote_id,
        call_id: CallId::from_bytes([0x46; 16]),
        operation_id: OperationId::from_bytes([0x47; 16]),
        idempotency_key: IdempotencyKey::from_bytes([0x48; 16]),
        reserved_at_ms: 2000,
    }) {
        Ok(nlos_resource::ReservationDecision::Replayed(r)) => r,
        other => panic!("expected replayed reserve, got {other:?}"),
    };
    // The reservation is already FINALIZED (the child settled it); the
    // replayed record carries the activation binding, so no re-activation.
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Active,
        "phantom finalize overlay must be rolled back"
    );
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900,
        "phantom refund must be rolled back"
    );

    // The interrupted decision is redoable: same deterministic receipt id
    // across a redo + reopen.
    settle(&authority, &reservation);
    let receipt = authority
        .inspect_finalize_receipt(reservation.reservation_id)
        .unwrap();
    assert_eq!(receipt.refund_credit, 60);
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    drop(authority);
    let reopened = ResourceAuthority::open(&root.path).expect("reopen after redo");
    assert_eq!(
        reopened
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap(),
        receipt
    );
    assert_integrity(&root.db());
}

// ---------------------------------------------------------------------------
// 矩阵行 2: kill-9 after commit — the committed finalize settlement survives
// ---------------------------------------------------------------------------

/// commit 后崩溃：子进程在完整 finalize 结算（receipt + FINALIZED overlay +
/// 账户退款）全部提交返回后被强杀；重开后 receipt 逐位保留、reservation
/// `FINALIZED`、账户 960；finalize 重放返回原 receipt（不重复退款）、receipt
/// 表 UPDATE/DELETE 被 immutable trigger 拒绝、重启回读一致。
#[test]
#[allow(clippy::too_many_lines)] // One test covers the full committed-settlement fault row.
fn fault_kill9_after_finalize_commit_preserves_settlement() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("kill9-finalize-commit");
    let mut child = spawn_child("finalize-commit-complete", &root.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 1);
    let authority = ResourceAuthority::open(&root.path).expect("reopen after kill");
    let account = authority.create_account(account_request()).unwrap();
    let driver = authority
        .register_driver(driver_request())
        .unwrap()
        .record();
    let quote = authority
        .create_quote(quote_request(driver))
        .unwrap()
        .record();
    let reservation = match authority.reserve(nlos_resource::ReserveRequest {
        account_id: account.account_id,
        quote_id: quote.quote_id,
        call_id: CallId::from_bytes([0x46; 16]),
        operation_id: OperationId::from_bytes([0x47; 16]),
        idempotency_key: IdempotencyKey::from_bytes([0x48; 16]),
        reserved_at_ms: 2000,
    }) {
        Ok(nlos_resource::ReservationDecision::Replayed(r)) => r,
        other => panic!("expected replayed reserve, got {other:?}"),
    };
    // The reservation is already FINALIZED (the child settled it); the
    // replayed record carries the activation binding, so no re-activation.

    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960,
        "the committed refund survives the crash"
    );
    let receipt = authority
        .inspect_finalize_receipt(reservation.reservation_id)
        .unwrap();
    assert_eq!(receipt.refund_credit, 60);
    assert_eq!(receipt.high_water, 40);

    // Replay returns the original receipt; no second refund is invented.
    match authority
        .finalize_reservation(finalize_request(&reservation))
        .unwrap()
    {
        FinalizeDecision::Replayed(replayed) => assert_eq!(replayed, receipt),
        other @ FinalizeDecision::Finalized(_) => panic!("expected Replayed, got {other:?}"),
    }
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960,
        "exact replay must not refund twice"
    );

    // The receipt and the overlay binding are immutable.
    let raw = Connection::open(root.db()).expect("raw connection");
    assert!(
        raw.execute(
            "UPDATE reservation_finalize_receipts SET refund_credit=0 WHERE receipt_id=?1",
            rusqlite::params![receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM reservation_finalize_receipts WHERE receipt_id=?1",
            rusqlite::params![receipt.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE reservations SET finalize_receipt_id=NULL WHERE reservation_id=?1",
            rusqlite::params![reservation.reservation_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    drop(raw);
    assert_integrity(&root.db());

    // Restart readback stays bit-identical.
    drop(authority);
    let reopened = ResourceAuthority::open(&root.path).expect("reopen after kill");
    assert_eq!(
        reopened
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap(),
        receipt
    );
    assert_eq!(
        reopened
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
}

// ---------------------------------------------------------------------------
// 矩阵行 3: hard I/O error on finalize fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误（finalize 结算事务）：`FailWritesAfter { 0, IoErr }` 下
/// `finalize_reservation` 以 `ResourceAuthorityError::Sqlite` 显式失败（错误
/// 链含 I/O 条件），不返回假成功；无半截状态（无 receipt 行、reservation
/// 保持 `ACTIVE`、账户保持 900、refund 不落账）；disarm 后同一 finalize
/// 成功且重启回读一致。
#[test]
fn fault_io_error_on_finalize_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("ioerr-finalize");
    let authority = open_shim(&root.path);
    let (account, reservation) = seed_active(&authority);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_reservation(finalize_request(&reservation))
        .expect_err("finalize must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_no_half_finalize(&authority, &root, account, &reservation);

    nlos_store_fault::disarm();
    settle(&authority, &reservation);
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    assert_integrity(&root.db());
    drop(authority);
    let reopened = ResourceAuthority::open(&root.path).expect("reopen");
    assert_eq!(
        reopened
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
}

// ---------------------------------------------------------------------------
// 矩阵行 4: disk-full (ENOSPC) on finalize fails closed
// ---------------------------------------------------------------------------

/// disk-full（finalize 结算事务）：`FailWritesAfter { 0, Full }` 下以
/// `SQLITE_FULL`（错误链含 full）显式失败且无半截状态；disarm 后同一
/// finalize 成功。
#[test]
fn fault_enospc_on_finalize_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("full-finalize");
    let authority = open_shim(&root.path);
    let (account, reservation) = seed_active(&authority);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_reservation(finalize_request(&reservation))
        .expect_err("finalize must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_no_half_finalize(&authority, &root, account, &reservation);

    nlos_store_fault::disarm();
    settle(&authority, &reservation);
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    assert_integrity(&root.db());
}

// ---------------------------------------------------------------------------
// 矩阵行 5: silent write loss / torn tail fabricates no phantom settlement
// ---------------------------------------------------------------------------

/// 静默丢写/撕裂尾部（finalize 表组）：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下 finalize "报告成功"但
///   写入从未落盘；重开后幻影 receipt/overlay/refund 不得冒充已提交事实
///   （reservation 保持 `ACTIVE`、无 receipt 行、账户 900），同一 finalize
///   重做且确定性派生 receipt id 逐位相同、重开后真实持久。
/// - Phase B（撕裂尾部）：子进程提交完整 finalize 后被强杀，父进程把 WAL
///   截断到最后一个 commit 帧（finalize 事务）的一半；重开后 finalize 整体
///   隐藏（reservation 保持 `ACTIVE`、账户 900、无 receipt 行），同一
///   finalize 重做且 receipt id 一致、退款 960 持久。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_phantom_finalize() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_finalize_and_redo_is_durable();
    torn_wal_tail_hides_finalize_and_redo_is_durable();
}

fn power_loss_drops_finalize_and_redo_is_durable() {
    let root = TestRoot::new("power-loss-finalize");
    let authority = open_shim(&root.path);
    let (account, reservation) = seed_active(&authority);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = finalize_receipt(
        authority
            .finalize_reservation(finalize_request(&reservation))
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = ResourceAuthority::open(&root.path).expect("reopen after power loss");
    assert_eq!(
        recovered
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Active,
        "silently dropped finalize must not move the reservation"
    );
    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 0);
    assert_eq!(
        recovered
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900,
        "silently dropped refund must not be durable"
    );
    assert_integrity(&root.db());

    // The lost decision is redoable: the deterministic receipt id is reused.
    let redone = finalize_receipt(
        recovered
            .finalize_reservation(finalize_request(&reservation))
            .expect("redo finalize after power loss"),
    );
    assert_eq!(redone.receipt_id, phantom.receipt_id);
    drop(recovered);
    let verified = ResourceAuthority::open(&root.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
    assert_eq!(
        verified
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    assert_integrity(&root.db());
    drop(verified);
    drop(root);
}

fn torn_wal_tail_hides_finalize_and_redo_is_durable() {
    let root = TestRoot::new("torn-tail-finalize");
    let mut child = spawn_child("torn-wal-finalize", &root.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&root.db());

    let recovered = ResourceAuthority::open(&root.path).expect("reopen after truncation");
    let account = recovered.create_account(account_request()).unwrap();
    let driver = recovered
        .register_driver(driver_request())
        .unwrap()
        .record();
    let quote = recovered
        .create_quote(quote_request(driver))
        .unwrap()
        .record();
    let reservation = match recovered.reserve(nlos_resource::ReserveRequest {
        account_id: account.account_id,
        quote_id: quote.quote_id,
        call_id: CallId::from_bytes([0x46; 16]),
        operation_id: OperationId::from_bytes([0x47; 16]),
        idempotency_key: IdempotencyKey::from_bytes([0x48; 16]),
        reserved_at_ms: 2000,
    }) {
        Ok(nlos_resource::ReservationDecision::Replayed(r)) => r,
        other => panic!("expected replayed reserve, got {other:?}"),
    };
    let reservation = activate(&recovered, reservation);
    assert_eq!(
        recovered
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Active,
        "torn finalize tail must not move the reservation"
    );
    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 0);
    assert_eq!(
        recovered
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        900,
        "torn finalize tail must not refund"
    );
    assert_integrity(&root.db());

    // The hidden finalize is redoable with the same deterministic receipt.
    settle(&recovered, &reservation);
    let receipt = recovered
        .inspect_finalize_receipt(reservation.reservation_id)
        .unwrap();
    drop(recovered);
    let verified = ResourceAuthority::open(&root.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_finalize_receipt(reservation.reservation_id)
            .unwrap(),
        receipt
    );
    assert_eq!(
        verified
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    assert_integrity(&root.db());
    drop(verified);
    drop(root);
}

// ---------------------------------------------------------------------------
// 矩阵行 6: after the fault clears, the finalize continues from the prefix
// ---------------------------------------------------------------------------

/// 故障解除后：finalize 写事务在 `FailWritesAfter { 0, Full }` 下失败后
/// disarm，同一 authority 实例继续读写——已提交前缀（reservation `ACTIVE`、
/// 账户 900、无 receipt）与故障前逐位一致；finalize 重试成功（`FINALIZED`、
/// 退款 960）；完整重开后全部状态可恢复。
#[test]
fn fault_after_disarm_finalize_continues_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("disarm-finalize-continue");
    let authority = open_shim(&root.path);
    let (account, reservation) = seed_active(&authority);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    authority
        .finalize_reservation(finalize_request(&reservation))
        .expect_err("finalize must fail while the fault is armed");
    assert_no_half_finalize(&authority, &root, account, &reservation);

    nlos_store_fault::disarm();
    settle(&authority, &reservation);
    assert_eq!(
        authority
            .inspect_account(account.account_id)
            .unwrap()
            .available_credit,
        960
    );
    drop(authority);

    let reopened = ResourceAuthority::open(&root.path).expect("reopen after recovery");
    assert_eq!(
        reopened
            .inspect_reservation(reservation.reservation_id)
            .unwrap()
            .state,
        ReservationState::Finalized
    );
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
    assert_eq!(raw_count(&root.db(), "reservation_finalize_receipts"), 1);
    assert_integrity(&root.db());
}
