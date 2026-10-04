//! Schema-v5 dead-letter parking acceptance tests (W57-B): one-way
//! parking, pending-lane skipping, inspect visibility, idempotent replay,
//! the typed conflict lanes, fail-closed request validation, and the
//! storage-layer one-way trigger.

use nlos_operation::{CompletionDecision, CompletionOutcome, OperationSpec};
use nlos_runtime::FiberHandle;
use nlos_store::{
    OutboxKind, OutboxParkDecision, RegistrationDecision, SqliteOperationStore, StoreError,
};
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, OperationId, ReceiptId,
};

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TestFile(std::path::PathBuf);

impl TestFile {
    fn new(name: &str) -> Self {
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-store-park-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        )))
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.0.display()));
        }
    }
}

fn spec(seed: u8) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes([seed; 16]),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(1); 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

/// Commits one unacknowledged outbox row per call and returns its durable
/// sequence (the newest row of the pending lane).
fn commit_wake_entry(store: &SqliteOperationStore, seed: u8) -> i64 {
    let handle = match store.register(spec(seed)).expect("register") {
        RegistrationDecision::Created(handle) => handle,
        RegistrationDecision::Existing(_) => panic!("fresh register cannot exist"),
    };
    let ticket = store
        .dispatch(handle, CallbackId::from_bytes([seed; 16]))
        .expect("dispatch");
    match store
        .complete(
            ticket,
            CompletionOutcome::Completed {
                receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x40); 16]),
            },
        )
        .expect("complete")
    {
        CompletionDecision::CanonicalizedAndWake { .. } => {}
        other => panic!("expected a wake canonicalization, got {other:?}"),
    }
    let pending = store.pending_outbox(100).expect("pending");
    pending.last().expect("the wake entry is pending").sequence
}

/// Given/When/Then: given two unacknowledged outbox rows; when the head is
/// parked; then the park stamps timestamp and reason, the pending lane
/// skips exactly the parked row (later rows flow), the inspect surface
/// lists the parked row with its facts and `acknowledged == false`, and
/// the parked facts survive a reopen.
#[test]
fn parked_row_leaves_pending_stays_inspectable_and_survives_reopen() {
    let database = TestFile::new("pending-skip");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let head = commit_wake_entry(&store, 0x11);
    let tail = commit_wake_entry(&store, 0x22);
    assert_eq!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        vec![head, tail]
    );

    let decision = store
        .park_outbox_entry(head, "poison: no route ever binds this entry", 1_000)
        .expect("park");
    assert_eq!(
        decision,
        OutboxParkDecision::Parked {
            sequence: head,
            parked_at_ms: 1_000,
        }
    );

    assert_eq!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        vec![tail],
        "the parked head no longer blocks the pending lane"
    );

    let parked = store.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].sequence, head);
    assert_eq!(parked[0].kind, OutboxKind::WakeFiber);
    assert!(!parked[0].acknowledged, "parking is not an acknowledgement");
    assert_eq!(parked[0].parked_at_ms, 1_000);
    assert_eq!(
        parked[0].park_reason,
        "poison: no route ever binds this entry"
    );

    drop(store);
    let reopened = SqliteOperationStore::open(&database.0).expect("reopen");
    let parked = reopened.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1, "the park is durable across reopen");
    assert_eq!(parked[0].parked_at_ms, 1_000);
}

/// Given/When/Then: given a parked row; when the exact same park request
/// is replayed; then the store answers the idempotent `Replayed` decision
/// with the original timestamp and leaves the row untouched; when a park
/// with a different reason arrives instead; then it fails closed with the
/// typed one-way conflict and the durable reason still reads the
/// original.
#[test]
fn park_is_idempotent_for_the_same_reason_and_conflicts_on_a_different_one() {
    let database = TestFile::new("park-idempotency");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x33);

    store
        .park_outbox_entry(sequence, "same diagnosis", 2_000)
        .expect("first park");

    let replay = store
        .park_outbox_entry(sequence, "same diagnosis", 9_999)
        .expect("replay");
    assert_eq!(
        replay,
        OutboxParkDecision::Replayed {
            sequence,
            parked_at_ms: 2_000,
        },
        "the replay returns the original timestamp, not the new one"
    );

    assert!(matches!(
        store.park_outbox_entry(sequence, "a different diagnosis", 3_000),
        Err(StoreError::OutboxParkConflict),
    ));

    let parked = store.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1);
    assert_eq!(parked[0].parked_at_ms, 2_000);
    assert_eq!(parked[0].park_reason, "same diagnosis");
}

/// Given/When/Then: given park requests of invalid shapes; when they are
/// offered; then each fails closed with the typed invalid-request error
/// and no durable state changes; an unknown sequence fails with the typed
/// not-found error.
#[test]
fn invalid_park_requests_fail_closed_without_touching_durable_state() {
    let database = TestFile::new("park-validation");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x44);

    for (reason, now_ms) in [("", 1_i64), ("fine reason", -1_i64)] {
        assert!(
            matches!(
                store.park_outbox_entry(sequence, reason, now_ms),
                Err(StoreError::InvalidParkRequest(_))
            ),
            "reason {reason:?} at now_ms {now_ms} must fail closed"
        );
    }
    assert!(matches!(
        store.park_outbox_entry(sequence, &"x".repeat(1025), 1),
        Err(StoreError::InvalidParkRequest(_))
    ));
    assert!(matches!(
        store.park_outbox_entry(sequence, "carries a \0 NUL", 1),
        Err(StoreError::InvalidParkRequest(_))
    ));
    // The boundary itself is legal: exactly 1024 bytes parks.
    let bounded = "y".repeat(1024);
    store
        .park_outbox_entry(sequence, &bounded, 1)
        .expect("1024 bytes is within the bound");

    assert!(matches!(
        store.park_outbox_entry(4_242, "unknown entry", 1),
        Err(StoreError::OutboxEntryNotFound)
    ));

    let parked = store.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1, "only the bounded park took effect");
}

/// Given/When/Then: given a parked row; when it is acknowledged through
/// the explicit adjudication path; then the ack commits (parking never
/// forbids adjudication), the pending lane still skips the row, and the
/// inspect surface reports the acknowledged flag truthfully.
#[test]
fn parked_row_can_still_be_acknowledged_by_explicit_adjudication() {
    let database = TestFile::new("park-then-ack");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x55);

    store
        .park_outbox_entry(sequence, "adjudicate: duplicate of seq 1", 5_000)
        .expect("park");
    store.acknowledge_outbox(sequence).expect("explicit ack");
    // The ack is idempotent, also on a parked row.
    store.acknowledge_outbox(sequence).expect("repeat ack");

    assert!(
        store.pending_outbox(10).expect("pending").is_empty(),
        "an acknowledged parked row never returns to pending"
    );
    let parked = store.inspect_parked_outbox(10).expect("inspect parked");
    assert_eq!(parked.len(), 1);
    assert!(
        parked[0].acknowledged,
        "the inspect surface reports the adjudication"
    );
    assert_eq!(parked[0].park_reason, "adjudicate: duplicate of seq 1");
}

/// Given/When/Then: given a parked row; when raw SQL tries to rewrite the
/// parking columns (un-park, or re-park with a new reason or timestamp);
/// then the schema-v5 trigger aborts every such UPDATE — parking is
/// one-way at the storage layer, not only through the store API.
#[test]
fn storage_trigger_makes_parking_strictly_one_way() {
    let database = TestFile::new("park-one-way-trigger");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x66);
    store
        .park_outbox_entry(sequence, "trigger evidence", 6_000)
        .expect("park");
    drop(store);

    let connection = rusqlite::Connection::open(&database.0).expect("raw open");
    for sql in [
        "UPDATE operation_outbox SET parked_at_ms = NULL, park_reason = NULL \
         WHERE sequence = ?1",
        "UPDATE operation_outbox SET parked_at_ms = 7_000 WHERE sequence = ?1",
        "UPDATE operation_outbox SET park_reason = 'rewritten' WHERE sequence = ?1",
        "UPDATE operation_outbox SET parked_at_ms = NULL WHERE sequence = ?1",
        "UPDATE operation_outbox SET park_reason = NULL WHERE sequence = ?1",
    ] {
        let result = connection.execute(sql, [sequence]);
        assert!(
            result.is_err(),
            "the trigger must abort a parking rewrite: {sql}"
        );
    }
    // The untouched `acknowledged` UPDATE lane still works: the trigger
    // scopes to the parking columns only.
    connection
        .execute(
            "UPDATE operation_outbox SET acknowledged = 1 WHERE sequence = ?1",
            [sequence],
        )
        .expect("acknowledging a parked row stays legal");
}
