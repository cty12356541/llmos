//! Explicit Stage B scale probe for 100K durable Operation metadata rows and
//! their pending Outbox entries. Kept ignored in the default suite because it
//! intentionally materializes a large database and performs durable ACKs.

mod support;

use std::time::{Duration, Instant};

use nlos_store::SqliteOperationStore;
use rusqlite::{Connection, TransactionBehavior, params};

use support::{TestFile, file_size};

const OPERATION_COUNT: usize = 100_000;
const ACK_SAMPLE: usize = 512;

fn id_bytes(domain: u64, value: usize) -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    bytes[..8].copy_from_slice(&domain.to_be_bytes());
    bytes[8..].copy_from_slice(&u64::try_from(value).expect("index fits u64").to_be_bytes());
    bytes
}

fn materialize_scale_fixture(path: &std::path::Path) -> Duration {
    // Let the authority create the current schema and durability pragmas.
    drop(SqliteOperationStore::open(path).expect("create current schema"));
    let mut connection = Connection::open(path).expect("open scale fixture");
    connection
        .pragma_update(None, "synchronous", "OFF")
        .expect("fixture load is rebuildable test data");
    let started = Instant::now();
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("begin fixture load");
    {
        let mut operation = transaction
            .prepare(
                "INSERT INTO operations (
                    operation_id, generation, owner_fiber_id, owner_fiber_generation,
                    cancellation_scope_id, cancellation_generation, cancel_epoch,
                    state_kind, receipt_id, revision
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 10, ?8, 1)",
            )
            .expect("prepare operation insert");
        let mut outbox = transaction
            .prepare(
                "INSERT INTO operation_outbox (
                    kind, operation_id, operation_generation, owner_fiber_id,
                    owner_fiber_generation, callback_id, state_kind, receipt_id
                 ) VALUES (0, ?1, ?2, ?3, ?4, NULL, 10, ?5)",
            )
            .expect("prepare outbox insert");
        let generation = 1_u64.to_be_bytes();
        let cancel_epoch = 0_u64.to_be_bytes();
        for index in 0..OPERATION_COUNT {
            let operation_id = id_bytes(1, index);
            let owner_fiber_id = id_bytes(2, index);
            let cancellation_scope_id = id_bytes(3, index);
            let receipt_id = id_bytes(4, index);
            operation
                .execute(params![
                    operation_id.as_slice(),
                    generation.as_slice(),
                    owner_fiber_id.as_slice(),
                    generation.as_slice(),
                    cancellation_scope_id.as_slice(),
                    generation.as_slice(),
                    cancel_epoch.as_slice(),
                    receipt_id.as_slice(),
                ])
                .expect("insert operation");
            outbox
                .execute(params![
                    operation_id.as_slice(),
                    generation.as_slice(),
                    owner_fiber_id.as_slice(),
                    generation.as_slice(),
                    receipt_id.as_slice(),
                ])
                .expect("insert outbox");
        }
    }
    transaction.commit().expect("commit fixture load");
    started.elapsed()
}

#[test]
#[ignore = "explicit Stage B 100K Operation metadata scale probe"]
fn one_hundred_thousand_operation_metadata_recovery_pending_and_ack() {
    let database = TestFile::new("metadata-100k");
    let load_elapsed = materialize_scale_fixture(&database.path);
    assert!(
        load_elapsed < Duration::from_mins(1),
        "fixture load: {load_elapsed:?}"
    );

    let open_started = Instant::now();
    let store = SqliteOperationStore::open(&database.path).expect("recover 100K database");
    let open_elapsed = open_started.elapsed();
    assert!(
        open_elapsed < Duration::from_secs(10),
        "open: {open_elapsed:?}"
    );

    let pending_started = Instant::now();
    let pending = store.pending_outbox(ACK_SAMPLE).expect("read pending page");
    let pending_elapsed = pending_started.elapsed();
    assert_eq!(pending.len(), ACK_SAMPLE);
    assert_eq!(pending.first().expect("first").sequence, 1);
    assert_eq!(pending.last().expect("last").sequence, 512);
    assert!(
        pending_elapsed < Duration::from_secs(10),
        "pending: {pending_elapsed:?}"
    );

    let ack_started = Instant::now();
    for entry in &pending {
        store
            .acknowledge_outbox(entry.sequence)
            .expect("durable ACK");
    }
    let ack_elapsed = ack_started.elapsed();
    assert!(
        ack_elapsed < Duration::from_mins(1),
        "ACK sample: {ack_elapsed:?}"
    );
    drop(store);

    let reopen_started = Instant::now();
    let recovered = SqliteOperationStore::open(&database.path).expect("reopen after ACKs");
    let reopen_elapsed = reopen_started.elapsed();
    let next = recovered.pending_outbox(1).expect("next pending entry");
    assert_eq!(next[0].sequence, 513);
    assert!(
        reopen_elapsed < Duration::from_secs(10),
        "reopen: {reopen_elapsed:?}"
    );

    let database_bytes = file_size(&database.path);
    eprintln!(
        "100K metadata profile: load={load_elapsed:?} open={open_elapsed:?} \
         pending512={pending_elapsed:?} ack512={ack_elapsed:?} \
         reopen={reopen_elapsed:?} database_bytes={database_bytes}"
    );
}
