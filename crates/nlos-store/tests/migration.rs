//! Schema v1→v6 migration, golden database, backup anchor, interrupted
//! migration acceptance, and the v4→v5 / v5→v6 parking upgrade tests.

mod support;

use std::sync::{Mutex, MutexGuard};

use nlos_operation::OperationState;
use nlos_store::{OutboxUnparkDecision, SqliteOperationStore};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{Generation, OperationId, ReceiptId};
use rusqlite::Connection;

use support::{TestFile, file_size, spec};

const GOLDEN_V1: &str = include_str!("fixtures/authority-v1.sql");
/// A complete v4 database (schema minus the v5 parking upgrade), assembled
/// by regressing nothing: the fixture SQL is the v1 golden plus the
/// v2/v3/v4 objects exactly as the store's own migrations create them.
const GOLDEN_V4: &str = include_str!("fixtures/authority-v4.sql");
/// A complete v5 database (schema minus the v6 controlled-unpark upgrade)
/// whose golden outbox row is parked under the v5 one-way schema, so the
/// v5→v6 path must both upgrade losslessly and backfill the parked row's
/// park count to exactly one.
const GOLDEN_V5: &str = include_str!("fixtures/authority-v5.sql");
const VFS_NAME: &str = "nlos-store-fault-migration";
static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn create_golden_v1(file: &TestFile) {
    let connection = Connection::open(&file.path).expect("create golden v1");
    connection.execute_batch(GOLDEN_V1).expect("load golden v1");
    assert!(file_size(&file.path) > 0, "golden database is materialized");
}

fn create_golden_v4(file: &TestFile) {
    let connection = Connection::open(&file.path).expect("create golden v4");
    connection.execute_batch(GOLDEN_V4).expect("load golden v4");
    assert!(file_size(&file.path) > 0, "golden database is materialized");
}

fn create_golden_v5(file: &TestFile) {
    let connection = Connection::open(&file.path).expect("create golden v5");
    connection.execute_batch(GOLDEN_V5).expect("load golden v5");
    assert!(file_size(&file.path) > 0, "golden database is materialized");
}

fn version(path: &std::path::Path) -> i64 {
    Connection::open(path)
        .expect("open raw database")
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version")
}

fn assert_v2_index(path: &std::path::Path) {
    let connection = Connection::open(path).expect("open migrated database");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'index' AND name = 'operation_outbox_by_operation'",
            [],
            |row| row.get(0),
        )
        .expect("query v2 index");
    assert_eq!(count, 1, "v2 recovery index exists exactly once");
}

fn assert_v3_idempotency_table(path: &std::path::Path) {
    let connection = Connection::open(path).expect("open migrated database");
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'idempotent_calls'",
            [],
            |row| row.get(0),
        )
        .expect("query v3 table");
    assert_eq!(count, 1, "v3 idempotency table exists exactly once");
}

/// The v5 parking upgrade left exactly one one-way trigger, exactly one
/// pending-unparked partial index, and the two nullable parking columns.
fn assert_v5_parking(path: &std::path::Path) {
    let connection = Connection::open(path).expect("open migrated database");
    let count = |object: &str, name: &str| -> i64 {
        connection
            .query_row(
                &format!(
                    "SELECT count(*) FROM sqlite_schema
                     WHERE type = '{object}' AND name = '{name}'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("query {name}: {error}"))
    };
    assert_eq!(
        count("trigger", "operation_outbox_parking_is_one_way"),
        1,
        "v5 one-way parking trigger exists exactly once"
    );
    assert_eq!(
        count("index", "operation_outbox_pending_unparked"),
        1,
        "v5 pending-unparked partial index exists exactly once"
    );
    let parked_columns: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('operation_outbox')
             WHERE name IN ('parked_at_ms', 'park_reason')",
            [],
            |row| row.get(0),
        )
        .expect("query v5 columns");
    assert_eq!(parked_columns, 2, "both v5 parking columns exist");
}

/// The v6 controlled-unpark upgrade left exactly one v6 parking trigger,
/// exactly one unpark-evidence trigger, and the three unpark columns.
fn assert_v6_unpark(path: &std::path::Path) {
    let connection = Connection::open(path).expect("open migrated database");
    let count = |object: &str, name: &str| -> i64 {
        connection
            .query_row(
                &format!(
                    "SELECT count(*) FROM sqlite_schema
                     WHERE type = '{object}' AND name = '{name}'"
                ),
                [],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("query {name}: {error}"))
    };
    assert_eq!(
        count("trigger", "operation_outbox_parking_is_one_way"),
        1,
        "the v6 parking trigger exists exactly once (replacing v5's)"
    );
    assert_eq!(
        count("trigger", "operation_outbox_unpark_evidence_is_one_way"),
        1,
        "v6 unpark-evidence trigger exists exactly once"
    );
    let unpark_columns: i64 = connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('operation_outbox')
             WHERE name IN ('unparked_at_ms', 'unpark_reason', 'park_count')",
            [],
            |row| row.get(0),
        )
        .expect("query v6 columns");
    assert_eq!(unpark_columns, 3, "all three v6 unpark columns exist");
}

/// The v5 golden's parked outbox row reads back verbatim after any
/// upgrade, with its park count derived as exactly one.
fn assert_golden_parked_row(store: &SqliteOperationStore) {
    let parked = store.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].sequence, 7);
    assert!(!parked[0].acknowledged);
    assert_eq!(parked[0].parked_at_ms, 6_000);
    assert_eq!(
        parked[0].park_reason,
        "v5 parked: poison head under the one-way schema"
    );
    let history = store
        .inspect_outbox_park_history_entry(7)
        .expect("park history")
        .expect("the parked golden row has history");
    assert!(history.parked);
    assert_eq!(
        history.park_count, 1,
        "the v5 park backfills to exactly one"
    );
    assert_eq!(history.unpark_count, 0);
}

fn assert_golden_operation(store: &SqliteOperationStore) {
    let handle = nlos_operation::OperationHandle {
        operation_id: OperationId::from_bytes([0x11; 16]),
        generation: Generation::INITIAL,
    };
    assert_eq!(
        store
            .inspect(handle)
            .expect("inspect golden operation")
            .state,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0x15; 16]),
        }
    );
}

fn assert_golden_data(store: &SqliteOperationStore) {
    assert_golden_operation(store);
    let outbox = store.pending_outbox(10).expect("golden outbox");
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].sequence, 7);
}

#[test]
fn golden_v1_migrates_forward_without_data_loss_and_accepts_new_writes() {
    let database = TestFile::new("migration-golden");
    create_golden_v1(&database);

    let store = SqliteOperationStore::open(&database.path).expect("migrate v1 to v6");
    assert_eq!(version(&database.path), 6);
    assert_v2_index(&database.path);
    assert_v3_idempotency_table(&database.path);
    assert_v5_parking(&database.path);
    assert_v6_unpark(&database.path);
    assert_golden_data(&store);
    store.register(spec(0x21)).expect("write after migration");
}

#[test]
fn pre_upgrade_backup_remains_a_restorable_v1_rollback_anchor() {
    let source = TestFile::new("migration-source");
    let backup_file = TestFile::new("migration-backup");
    create_golden_v1(&source);

    let source_connection = Connection::open(&source.path).expect("open v1 source");
    let mut backup_connection = Connection::open(&backup_file.path).expect("open backup target");
    rusqlite::backup::Backup::new(&source_connection, &mut backup_connection)
        .expect("start pre-upgrade backup")
        .run_to_completion(8, std::time::Duration::ZERO, None)
        .expect("finish pre-upgrade backup");
    drop(backup_connection);
    drop(source_connection);

    let migrated = SqliteOperationStore::open(&source.path).expect("migrate source");
    assert_golden_data(&migrated);
    assert_eq!(version(&source.path), 6);
    assert_eq!(version(&backup_file.path), 1, "backup stays at v1");

    let restored =
        SqliteOperationStore::open(&backup_file.path).expect("restore and migrate backup");
    assert_golden_data(&restored);
    assert_eq!(version(&backup_file.path), 6);
}

#[test]
fn interrupted_migration_leaves_only_complete_schema_versions() {
    let _serialization = fault_lock();
    nlos_store_fault::register(VFS_NAME).expect("register migration fault VFS");

    let mut failure_points = 0_u64;
    loop {
        let database = TestFile::new("migration-interrupt");
        create_golden_v1(&database);
        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: failure_points,
            code: FaultCode::IoErr,
        });
        let result = SqliteOperationStore::open_with_vfs(&database.path, Some(VFS_NAME));
        nlos_store_fault::disarm();

        if let Ok(store) = result {
            assert_eq!(version(&database.path), 6);
            assert_v2_index(&database.path);
            assert_v3_idempotency_table(&database.path);
            assert_v5_parking(&database.path);
            assert_v6_unpark(&database.path);
            assert_golden_data(&store);
            break;
        }
        let durable_version = version(&database.path);
        assert!(
            matches!(durable_version, 1..=6),
            "interrupted migration exposed version {durable_version}"
        );
        if durable_version == 2 {
            assert_v2_index(&database.path);
        }
        if durable_version == 3 {
            assert_v2_index(&database.path);
            assert_v3_idempotency_table(&database.path);
        }
        if durable_version == 4 {
            assert_v2_index(&database.path);
            assert_v3_idempotency_table(&database.path);
        }
        if durable_version == 5 {
            assert_v2_index(&database.path);
            assert_v3_idempotency_table(&database.path);
            assert_v5_parking(&database.path);
        }
        failure_points += 1;
        assert!(
            failure_points < 64,
            "migration never reached a success point"
        );
    }
}

/// Given/When/Then: given a complete v4 database carrying one golden
/// operation and its unacknowledged outbox row; when it is opened (the v4→v5
/// upgrade path); then the upgrade is lossless and idempotent — the row and
/// its acknowledgement state survive verbatim, `pending_outbox` still serves
/// it, the v5 objects exist exactly once, `user_version` reads 5 (and then
/// 6 after the v5→v6 step of the same open), and a second open neither
/// re-runs a migration nor changes any fact.
#[test]
fn v4_upgrade_is_lossless_and_reopen_idempotent() {
    let database = TestFile::new("migration-v4");
    create_golden_v4(&database);
    assert_eq!(version(&database.path), 4);

    let store = SqliteOperationStore::open(&database.path).expect("upgrade v4 to current");
    assert_eq!(version(&database.path), 6);
    assert_v5_parking(&database.path);
    assert_v6_unpark(&database.path);
    assert_golden_data(&store);
    drop(store);

    // Reopening a current database must be a no-op upgrade with the same
    // facts.
    let reopened = SqliteOperationStore::open(&database.path).expect("reopen at current");
    assert_eq!(version(&database.path), 6);
    assert_v5_parking(&database.path);
    assert_v6_unpark(&database.path);
    assert_golden_data(&reopened);
    reopened
        .acknowledge_outbox(7)
        .expect("upgraded rows stay writable");
    assert_eq!(
        reopened.pending_outbox(10).expect("pending").len(),
        0,
        "the acknowledged golden row is consumed"
    );
}

/// Given/When/Then: given a complete v5 database whose golden outbox row is
/// parked under the v5 one-way schema; when it is opened (the v5→v6
/// controlled-unpark upgrade path); then the upgrade is lossless and
/// idempotent — the parked row survives verbatim (timestamp, reason,
/// unacknowledged), its park count backfills to exactly one, the pending
/// lane keeps skipping it, the v6 objects exist exactly once, `user_version`
/// reads 6, and a second open changes no fact. The legacy parked row is
/// then fully servable by the v6 lane: an unpark recovers it into the
/// pending lane and a redelivery acknowledgement consumes it.
#[test]
fn v5_upgrade_is_lossless_reopen_idempotent_and_serves_unpark() {
    let database = TestFile::new("migration-v5");
    create_golden_v5(&database);
    assert_eq!(version(&database.path), 5);

    let store = SqliteOperationStore::open(&database.path).expect("upgrade v5 to v6");
    assert_eq!(version(&database.path), 6);
    assert_v6_unpark(&database.path);
    // The golden operation survives verbatim; its outbox row is parked, so
    // the pending-lane assertion of the v1/v4 goldens does not apply here.
    assert_golden_operation(&store);
    assert_golden_parked_row(&store);
    assert_eq!(
        store.pending_outbox(10).expect("pending").len(),
        0,
        "the parked golden row stays out of the pending lane"
    );
    drop(store);

    // Reopening a v6 database must be a no-op upgrade with the same facts.
    let reopened = SqliteOperationStore::open(&database.path).expect("reopen at v6");
    assert_eq!(version(&database.path), 6);
    assert_v6_unpark(&database.path);
    assert_golden_parked_row(&reopened);

    // The v6 lane fully serves the legacy parked row: unpark recovers it,
    // the pending lane redelivers it in order, and the acknowledgement
    // consumes it. The unpark replay stays idempotent afterwards.
    let decision = reopened
        .unpark_outbox_entry(7, "adjudicated after the v6 upgrade", 8_000)
        .expect("unpark the legacy parked row");
    assert_eq!(
        decision,
        OutboxUnparkDecision::Unparked {
            sequence: 7,
            unparked_at_ms: 8_000,
        }
    );
    let pending = reopened.pending_outbox(10).expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].sequence, 7, "the recovered row is redelivered");
    let replay = reopened
        .unpark_outbox_entry(7, "adjudicated after the v6 upgrade", 9_999)
        .expect("unpark replay");
    assert_eq!(
        replay,
        OutboxUnparkDecision::Replayed {
            sequence: 7,
            unparked_at_ms: 8_000,
        }
    );
    reopened.acknowledge_outbox(7).expect("ack redelivery");
    assert_eq!(
        reopened.pending_outbox(10).expect("pending").len(),
        0,
        "the acknowledged recovery is consumed"
    );
    let history = reopened
        .inspect_outbox_park_history(10)
        .expect("park history");
    assert_eq!(history.len(), 1);
    assert!(!history[0].parked);
    assert!(history[0].acknowledged);
    assert_eq!(history[0].park_count, 1);
    assert_eq!(history[0].unpark_count, 1);
}
