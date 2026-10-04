//! Schema-v6 controlled unpark acceptance tests (W58-1): the adjudication
//! reverse of W57-B's one-way parking. One transaction walks a parked row
//! back into the pending lane in durable sequence order, keeps the unpark
//! evidence one-way, replays idempotently, refuses unparked rows with the
//! typed rejection lane, keeps the audit counts truthful across re-park
//! cycles, and the storage triggers permit only the fully-shaped reverse.

use nlos_operation::{CompletionDecision, CompletionOutcome, OperationSpec};
use nlos_runtime::FiberHandle;
use nlos_store::{
    OutboxKind, OutboxUnparkDecision, RegistrationDecision, SqliteOperationStore, StoreError,
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
            "nlos-store-unpark-{name}-{}-{sequence}.sqlite3",
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

/// Given/When/Then: given a parked head with a healthy entry queued behind
/// it; when a human adjudication unparks the head; then the controlled
/// reverse commits — the pending lane serves both entries again with the
/// recovered head first (in-order queue-head restoration), the consumer
/// redelivers and acknowledges it, the parked listing is empty while the
/// history keeps the adjudication facts (status recovered, one park, one
/// unpark), and everything survives a reopen.
#[test]
fn unparked_head_rejoins_pending_in_order_and_redelivers() {
    let database = TestFile::new("unpark-redeliver");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let head = commit_wake_entry(&store, 0x11);
    let tail = commit_wake_entry(&store, 0x22);
    store
        .park_outbox_entry(head, "poison: consumer cannot apply this yet", 1_000)
        .expect("park");
    assert_eq!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        vec![tail],
        "the parked head blocks the lane before the adjudication"
    );

    let decision = store
        .unpark_outbox_entry(head, "adjudicated: route restored, retry delivery", 2_000)
        .expect("unpark");
    assert_eq!(
        decision,
        OutboxUnparkDecision::Unparked {
            sequence: head,
            unparked_at_ms: 2_000,
        }
    );

    // The queue head restores in order: the recovered head is pending
    // again, ahead of the entry that kept flowing while it was parked.
    assert_eq!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        vec![head, tail],
        "the recovered head rejoins the pending lane at its durable position"
    );

    // The redelivery applies and acknowledges exactly like a fresh entry.
    let redelivered = &store.pending_outbox(10).expect("pending")[0];
    assert_eq!(redelivered.sequence, head);
    assert_eq!(redelivered.kind, OutboxKind::WakeFiber);
    store.acknowledge_outbox(head).expect("ack redelivery");
    assert_eq!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .map(|entry| entry.sequence)
            .collect::<Vec<_>>(),
        vec![tail],
        "the acknowledged recovery leaves the pending lane"
    );

    assert!(
        store.inspect_parked_outbox(10).expect("parked").is_empty(),
        "no entry is parked anymore"
    );
    let history = store.inspect_outbox_park_history(10).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].sequence, head);
    assert!(!history[0].parked, "the status reads recovered");
    assert!(history[0].acknowledged, "the redelivery was acknowledged");
    assert_eq!(history[0].park_count, 1);
    assert_eq!(history[0].unpark_count, 1);
    assert_eq!(history[0].parked_at_ms, None);
    assert_eq!(history[0].park_reason, None);
    assert_eq!(history[0].unparked_at_ms, Some(2_000));
    assert_eq!(
        history[0].unpark_reason.as_deref(),
        Some("adjudicated: route restored, retry delivery")
    );

    drop(store);
    let reopened = SqliteOperationStore::open(&database.0).expect("reopen");
    let history = reopened.inspect_outbox_park_history(10).expect("history");
    assert_eq!(history.len(), 1, "the recovery is durable across reopen");
    assert_eq!(history[0].unparked_at_ms, Some(2_000));
}

/// Given/When/Then: given unpark requests against unknown, never-parked,
/// and already-unparked rows, and requests of invalid shapes; when they
/// are offered; then the unknown sequence fails with the typed not-found
/// error, the never-parked row fails closed with the typed request
/// refusal, the invalid shapes fail before any row is touched, the exact
/// same-reason replay of a committed unpark answers `Replayed` with the
/// original timestamp, and a different reason is refused one-way — the
/// durable row never changes across any refusal.
#[test]
fn unpark_replays_idempotently_and_refuses_unparked_rows() {
    let database = TestFile::new("unpark-idempotency");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let parked = commit_wake_entry(&store, 0x33);
    let fresh = commit_wake_entry(&store, 0x44);

    assert!(matches!(
        store.unpark_outbox_entry(4_242, "unknown entry", 1),
        Err(StoreError::OutboxEntryNotFound)
    ));
    assert!(matches!(
        store.unpark_outbox_entry(fresh, "never parked", 1),
        Err(StoreError::InvalidParkRequest(_))
    ));
    for (reason, now_ms) in [("", 1_i64), ("fine reason", -1_i64)] {
        assert!(
            matches!(
                store.unpark_outbox_entry(parked, reason, now_ms),
                Err(StoreError::InvalidParkRequest(_))
            ),
            "reason {reason:?} at now_ms {now_ms} must fail closed"
        );
    }
    assert!(matches!(
        store.unpark_outbox_entry(parked, &"x".repeat(1025), 1),
        Err(StoreError::InvalidParkRequest(_))
    ));
    assert!(matches!(
        store.unpark_outbox_entry(parked, "carries a \0 NUL", 1),
        Err(StoreError::InvalidParkRequest(_))
    ));
    // The boundary itself is legal: exactly 1024 bytes unparks.
    let bounded = "y".repeat(1024);
    store
        .park_outbox_entry(parked, "diagnosis", 3_000)
        .expect("park");
    store
        .unpark_outbox_entry(parked, &bounded, 4_000)
        .expect("1024 bytes is within the bound");

    // Idempotent replay: the same reason answers with the original
    // timestamp; a different reason is the one-way refusal.
    let replay = store
        .unpark_outbox_entry(parked, &bounded, 9_999)
        .expect("replay");
    assert_eq!(
        replay,
        OutboxUnparkDecision::Replayed {
            sequence: parked,
            unparked_at_ms: 4_000,
        },
        "the replay returns the original adjudication timestamp, not the new one"
    );
    assert!(matches!(
        store.unpark_outbox_entry(parked, "a different adjudication", 5_000),
        Err(StoreError::InvalidParkRequest(_))
    ));

    // A refusal never mutates the durable row.
    let history = store.inspect_outbox_park_history(10).expect("history");
    assert_eq!(history.len(), 1);
    assert!(!history[0].parked);
    assert_eq!(history[0].unparked_at_ms, Some(4_000));
    assert_eq!(history[0].unpark_reason.as_deref(), Some(bounded.as_str()));
    assert_eq!(history[0].unpark_count, 1);
    // And a second unpark of the already-recovered row is still the typed
    // one-way lane, not a fresh reverse.
    assert!(matches!(
        store.unpark_outbox_entry(parked, "another try", 6_000),
        Err(StoreError::InvalidParkRequest(_))
    ));
}

/// Given/When/Then: given an entry walked through a full park/unpark
/// cycle; when it is re-parked and unparked again; then each cycle
/// advances the audit counts exactly once, the fresh park API replays
/// idempotently within its own cycle (same reason, original timestamp),
/// the latest park facts answer the currently parked state while the
/// latest unpark facts survive re-parks as adjudication evidence, and the
/// pending lane reflects the currently parked state at every step.
#[test]
fn audit_counts_track_full_re_park_cycles() {
    let database = TestFile::new("unpark-cycles");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x55);

    store
        .park_outbox_entry(sequence, "first diagnosis", 1_000)
        .expect("first park");
    store
        .unpark_outbox_entry(sequence, "first adjudication", 2_000)
        .expect("first unpark");

    // Re-park with a fresh reason is a fresh decision (the reverse opened
    // the lane again); replaying that same reason stays idempotent.
    store
        .park_outbox_entry(sequence, "second diagnosis", 3_000)
        .expect("second park");
    let replay = store
        .park_outbox_entry(sequence, "second diagnosis", 9_999)
        .expect("park replay");
    assert_eq!(
        replay,
        nlos_store::OutboxParkDecision::Replayed {
            sequence,
            parked_at_ms: 3_000,
        }
    );

    let history = store.inspect_outbox_park_history(10).expect("history");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].sequence, sequence);
    assert!(history[0].parked, "the entry is parked again");
    assert_eq!(history[0].park_count, 2, "both parks are counted");
    assert_eq!(
        history[0].unpark_count, 1,
        "the closed cycle is counted, the open park is not"
    );
    assert_eq!(history[0].parked_at_ms, Some(3_000));
    assert_eq!(history[0].park_reason.as_deref(), Some("second diagnosis"));
    assert_eq!(
        history[0].unparked_at_ms,
        Some(2_000),
        "the earlier adjudication survives the re-park"
    );
    assert_eq!(
        history[0].unpark_reason.as_deref(),
        Some("first adjudication")
    );
    assert!(
        store
            .pending_outbox(10)
            .expect("pending")
            .iter()
            .all(|entry| entry.sequence != sequence),
        "the re-parked head leaves the pending lane again"
    );

    store
        .unpark_outbox_entry(sequence, "second adjudication", 4_000)
        .expect("second unpark");
    let history = store.inspect_outbox_park_history(10).expect("history");
    assert!(!history[0].parked);
    assert_eq!(history[0].park_count, 2);
    assert_eq!(history[0].unpark_count, 2);
    assert_eq!(history[0].unparked_at_ms, Some(4_000));
    assert_eq!(
        history[0].unpark_reason.as_deref(),
        Some("second adjudication")
    );
    // The targeted history read answers the same facts for one sequence.
    let targeted = store
        .inspect_outbox_park_history_entry(sequence)
        .expect("targeted history")
        .expect("the entry exists");
    assert_eq!(targeted, history[0]);
    assert!(
        store
            .inspect_outbox_park_history_entry(4_242)
            .expect("targeted history")
            .is_none(),
        "an unknown sequence has no history row"
    );
}

/// Given/When/Then: given a row parked and then acknowledged by explicit
/// adjudication (the W57-B terminal lane); when it is unparked; then the
/// unpark commits (the two lifecycles stay orthogonal), but the
/// acknowledged row never re-enters the pending lane — recovery redelivery
/// is a pending-lane property, not an ack reset — and the history reports
/// both facts truthfully.
#[test]
fn acknowledged_parked_row_unparks_without_reentering_pending() {
    let database = TestFile::new("unpark-acked");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let sequence = commit_wake_entry(&store, 0x66);
    store
        .park_outbox_entry(sequence, "adjudicate: duplicate of seq 1", 5_000)
        .expect("park");
    store.acknowledge_outbox(sequence).expect("explicit ack");

    store
        .unpark_outbox_entry(sequence, "audit trail: unpark for the record", 6_000)
        .expect("unpark of an acknowledged parked row");

    assert!(
        store.pending_outbox(10).expect("pending").is_empty(),
        "an acknowledged row never returns to pending, unparked or not"
    );
    let history = store.inspect_outbox_park_history(10).expect("history");
    assert_eq!(history.len(), 1);
    assert!(!history[0].parked);
    assert!(history[0].acknowledged);
    assert_eq!(history[0].unpark_count, 1);
}

/// Given/When/Then: given parked, recovered, and fresh rows; when raw SQL
/// tries every rewrite of the parking facts except the fully-shaped
/// reverse; then the schema-v6 triggers abort each one — the bare
/// value→NULL back-out of a parked row, the reverse with partial evidence,
/// the v5 in-place rewrite lanes, a park without its count, a count without
/// a park, unpark evidence forged on a non-parked row, and unpark evidence
/// cleared on a recovered row — while the full reverse shape (both parking
/// columns to NULL plus both evidence columns stamped in the same
/// statement) commits, and the untouched `acknowledged` UPDATE lane stays
/// legal. (Writing a value that a column already holds is a no-op the
/// triggers rightly ignore; and restamping unpark evidence on a still
/// parked row cannot un-park it, so the triggers scope it out exactly like
/// v5 scoped out a raw fresh park: they shape state transitions, they do
/// not authenticate callers.)
#[allow(clippy::too_many_lines)] // One trigger matrix, one deterministic shape per lane.
#[test]
fn storage_triggers_permit_only_the_fully_shaped_reverse() {
    let database = TestFile::new("unpark-trigger");
    let store = SqliteOperationStore::open(&database.0).expect("open");
    let parked = commit_wake_entry(&store, 0x77);
    let recovered = commit_wake_entry(&store, 0x88);
    let fresh = commit_wake_entry(&store, 0x99);
    store
        .park_outbox_entry(parked, "trigger evidence", 7_000)
        .expect("park");
    store
        .park_outbox_entry(recovered, "first diagnosis", 7_100)
        .expect("park");
    store
        .unpark_outbox_entry(recovered, "first adjudication", 7_200)
        .expect("unpark");
    drop(store);

    let connection = rusqlite::Connection::open(&database.0).expect("raw open");
    let must_abort = |sql: &str, sequence: i64, why: &str| {
        let result = connection.execute(sql, [sequence]);
        assert!(
            result.is_err(),
            "the trigger must abort {why} (sequence {sequence}): {sql}"
        );
    };

    // The bare v5-style un-park of a parked row: no adjudication evidence.
    must_abort(
        "UPDATE operation_outbox SET parked_at_ms = NULL, park_reason = NULL \
         WHERE sequence = ?1",
        parked,
        "the value->NULL back-out without evidence",
    );
    // The reverse with only partial evidence, on every starting state.
    for sequence in [parked, recovered, fresh] {
        must_abort(
            "UPDATE operation_outbox SET parked_at_ms = NULL, park_reason = NULL, \
             unparked_at_ms = 9_000 WHERE sequence = ?1",
            sequence,
            "the reverse with partial evidence",
        );
    }
    // The in-place rewrite lanes of v5 stay dead on a parked row.
    must_abort(
        "UPDATE operation_outbox SET parked_at_ms = 8_000 WHERE sequence = ?1",
        parked,
        "a parked-timestamp rewrite",
    );
    must_abort(
        "UPDATE operation_outbox SET park_reason = 'rewritten' WHERE sequence = ?1",
        parked,
        "a park-reason rewrite",
    );
    must_abort(
        "UPDATE operation_outbox SET parked_at_ms = NULL WHERE sequence = ?1",
        parked,
        "a one-column un-park",
    );
    // A park must advance its count, and a count must ride a park.
    must_abort(
        "UPDATE operation_outbox SET parked_at_ms = 1, park_reason = 'x' \
         WHERE sequence = ?1",
        fresh,
        "a park without advancing the count",
    );
    must_abort(
        "UPDATE operation_outbox SET park_count = park_count + 1 WHERE sequence = ?1",
        fresh,
        "a count advance without a park",
    );
    // Unpark evidence is one-way outside a parked row's controlled reverse.
    for sequence in [recovered, fresh] {
        must_abort(
            "UPDATE operation_outbox SET unparked_at_ms = 9_000, unpark_reason = 'forge' \
             WHERE sequence = ?1",
            sequence,
            "unpark evidence forged on a non-parked row",
        );
    }
    must_abort(
        "UPDATE operation_outbox SET unparked_at_ms = NULL, unpark_reason = NULL \
         WHERE sequence = ?1",
        recovered,
        "unpark evidence cleared on a recovered row",
    );

    // The fully-shaped reverse is exactly the store's own UPDATE: the
    // trigger models the lane's shape, not the caller's identity.
    connection
        .execute(
            "UPDATE operation_outbox
             SET parked_at_ms = NULL, park_reason = NULL,
                 unparked_at_ms = 9_500, unpark_reason = 'raw but fully shaped'
             WHERE sequence = ?1",
            [parked],
        )
        .expect("the fully-shaped reverse is the one legal back-out");
    // The untouched `acknowledged` UPDATE lane still works: the triggers
    // scope to the parking-family columns only.
    connection
        .execute(
            "UPDATE operation_outbox SET acknowledged = 1 WHERE sequence = ?1",
            [recovered],
        )
        .expect("acknowledging a recovered row stays legal");

    let store = SqliteOperationStore::open(&database.0).expect("reopen");
    let history = store
        .inspect_outbox_park_history_entry(parked)
        .expect("history")
        .expect("the raw reverse left the audit facts");
    assert!(!history.parked);
    assert_eq!(history.unparked_at_ms, Some(9_500));
    assert_eq!(
        history.unpark_reason.as_deref(),
        Some("raw but fully shaped")
    );
}
