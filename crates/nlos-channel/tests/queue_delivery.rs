use nlos_channel::{
    AckDecision, AckReceipt, AckRequest, ChannelAuthority, ChannelAuthorityError, ChannelDecision,
    ChannelRecord, ChannelRotationDecision, CompactDecision, CompactReceipt, CreateChannelRequest,
    EnqueueDecision, EnqueueRequest, QueueEntryRecord, QueueState, RotateChannelRequest,
};
use nlos_types::IdempotencyKey;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-channel-queue-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> PathBuf {
        self.0.join("channel-authority.db")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn create(authority: &ChannelAuthority, capacity_bytes: u64, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 1_000,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn request_for(head: &ChannelRecord, seed: u8, payload: &[u8], at: u64) -> EnqueueRequest {
    EnqueueRequest {
        channel_id: head.channel_id,
        expected_generation: head.generation,
        expected_fencing_token: head.fencing_token,
        payload: payload.to_vec(),
        idempotency_key: key(seed),
        enqueued_at_ms: at,
    }
}

fn enqueue(
    authority: &ChannelAuthority,
    head: &ChannelRecord,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> QueueEntryRecord {
    match authority
        .enqueue(request_for(head, seed, payload, at))
        .expect("enqueue")
    {
        EnqueueDecision::Enqueued(entry) => entry,
        EnqueueDecision::Replayed(_) => panic!("fresh enqueue cannot replay"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full ordered delivery cycle.
fn enqueue_receive_ack_cycle_is_ordered_and_drains() {
    let root = Root::new("cycle");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 4_096, 200);
    let first = enqueue(&authority, &head, 1, b"first", 1_100);
    let second = enqueue(&authority, &head, 2, b"second", 1_101);
    let third = enqueue(&authority, &head, 3, b"third", 1_102);
    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    assert_eq!(third.sequence, 3);

    let window = authority.receive(head.channel_id, 10).expect("receive");
    assert_eq!(window, vec![first.clone(), second.clone(), third.clone()]);
    // Receive advances no cursor and writes nothing: repeating it (even with
    // a smaller limit) returns the same prefix of the same window.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive again"),
        window
    );
    assert_eq!(
        authority
            .receive(head.channel_id, 2)
            .expect("limited receive"),
        vec![first.clone(), second.clone()]
    );

    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 2,
            acked_at_ms: 1_200,
        })
        .expect("ack prefix");
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after ack"),
        vec![third.clone()]
    );
    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 1_201,
        })
        .expect("ack rest");
    assert!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive drained")
            .is_empty()
    );
    assert_eq!(
        authority
            .inspect_queue(head.channel_id)
            .expect("inspect queue"),
        QueueState {
            channel_id: head.channel_id,
            capacity_bytes: 4_096,
            consume_high_water: 3,
            trim_high_water: 0,
            backlog_bytes: 0,
            retained_bytes: 16,
            max_sequence: 3,
        }
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the admission boundary and zero-partial-state proof.
fn queue_full_admission_rejects_before_any_durable_write() {
    let root = Root::new("queue-full");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 10, 200);

    // Empty payloads are rejected before any durable write, mirroring the
    // zero-capacity pre-write rejection.
    assert!(matches!(
        authority.enqueue(EnqueueRequest {
            payload: Vec::new(),
            ..request_for(&head, 9, b"x", 1)
        }),
        Err(ChannelAuthorityError::InvalidPayload)
    ));

    assert!(matches!(
        authority
            .enqueue(request_for(&head, 1, b"aaaaaa", 10))
            .expect("first enqueue"),
        EnqueueDecision::Enqueued(_)
    ));
    // A backlog exactly equal to capacity is admitted.
    let boundary = match authority
        .enqueue(request_for(&head, 2, b"bbbb", 11))
        .expect("boundary enqueue")
    {
        EnqueueDecision::Enqueued(entry) => entry,
        EnqueueDecision::Replayed(_) => panic!("boundary enqueue cannot replay"),
    };
    assert_eq!(boundary.sequence, 2);
    // One byte over capacity is rejected...
    assert!(matches!(
        authority.enqueue(request_for(&head, 9, b"c", 12)),
        Err(ChannelAuthorityError::QueueFull)
    ));
    // ...with zero partial state: the window, the bookkeeping and the
    // rejected request's idempotency slot are all untouched.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after rejection")
            .len(),
        2
    );
    let state = authority
        .inspect_queue(head.channel_id)
        .expect("inspect after rejection");
    assert_eq!((state.backlog_bytes, state.retained_bytes), (10, 10));

    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 2,
            acked_at_ms: 20,
        })
        .expect("drain backlog");
    // Retrying the previously rejected request now succeeds as a fresh
    // Enqueued (not Replayed): the rejection left no durable trace.
    match authority
        .enqueue(request_for(&head, 9, b"c", 12))
        .expect("retry after drain")
    {
        EnqueueDecision::Enqueued(entry) => assert_eq!(entry.sequence, 3),
        EnqueueDecision::Replayed(_) => panic!("rejected enqueue must not leave a replay record"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers rotation fence + cross-generation read order.
fn stale_fence_rejects_enqueue_but_old_generation_entries_stay_receivable() {
    let root = Root::new("stale-fence");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    let old_entry = enqueue(&authority, &head, 1, b"gen-one", 10);

    let rotated = match authority
        .rotate_channel(RotateChannelRequest {
            channel_id: head.channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            idempotency_key: key(50),
            rotated_at_ms: 2_000,
        })
        .expect("rotate")
    {
        ChannelRotationDecision::Rotated(record) => record,
        ChannelRotationDecision::Replayed(_) => panic!("fresh rotate cannot replay"),
    };

    assert!(matches!(
        authority.enqueue(request_for(&head, 2, b"stale", 30)),
        Err(ChannelAuthorityError::StaleChannel)
    ));
    let new_entry = enqueue(&authority, &rotated, 3, b"gen-two", 31);
    assert_eq!(new_entry.sequence, 2);
    assert_eq!(new_entry.generation, rotated.generation);

    // The read path is a per-channel total order across generations: the
    // unconsumed generation-1 entry is still receivable after rotation.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive across generations"),
        vec![old_entry.clone(), new_entry.clone()]
    );
    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 2,
            acked_at_ms: 40,
        })
        .expect("ack across generations");
    assert_eq!(
        authority
            .compact(head.channel_id, 2)
            .expect("compact both generations"),
        CompactDecision::Trimmed(CompactReceipt {
            channel_id: head.channel_id,
            trim_high_water: 2,
        })
    );
    assert!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive drained")
            .is_empty()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers replay plus payload/fence drift conflicts.
fn enqueue_idempotency_replays_and_conflicts_on_payload_or_fence_drift() {
    let root = Root::new("idempotency");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    let original = enqueue(&authority, &head, 5, b"payload", 10);

    match authority
        .enqueue(request_for(&head, 5, b"payload", 10))
        .expect("exact replay")
    {
        EnqueueDecision::Replayed(entry) => assert_eq!(entry, original),
        EnqueueDecision::Enqueued(_) => panic!("exact replay must not enqueue again"),
    }

    // Same key with a drifted payload is a conflict.
    assert!(matches!(
        authority.enqueue(request_for(&head, 5, b"drift", 10)),
        Err(ChannelAuthorityError::IdempotencyConflict)
    ));
    // Same payload under a different key is a distinct entry.
    let sibling = enqueue(&authority, &head, 6, b"payload", 11);
    assert_eq!(sibling.sequence, 2);

    let rotated = authority
        .rotate_channel(RotateChannelRequest {
            channel_id: head.channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            idempotency_key: key(50),
            rotated_at_ms: 2_000,
        })
        .expect("rotate")
        .record();
    // The original request still replays after rotation because its fence
    // matches the stored entry.
    assert!(matches!(
        authority.enqueue(request_for(&head, 5, b"payload", 10)),
        Ok(EnqueueDecision::Replayed(_))
    ));
    // The same key presented with the new fence conflicts with the stored
    // entry.
    assert!(matches!(
        authority.enqueue(request_for(&rotated, 5, b"payload", 10)),
        Err(ChannelAuthorityError::IdempotencyConflict)
    ));
}

#[test]
fn ack_is_monotonic_idempotent_and_bounded_by_durable_maximum() {
    let root = Root::new("ack-bounds");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    enqueue(&authority, &head, 1, b"one", 10);
    enqueue(&authority, &head, 2, b"two", 11);

    assert_eq!(
        authority
            .ack(AckRequest {
                channel_id: head.channel_id,
                up_to_sequence: 1,
                acked_at_ms: 100,
            })
            .expect("first ack"),
        AckDecision::Advanced(AckReceipt {
            channel_id: head.channel_id,
            consume_high_water: 1,
            acked_at_ms: 100,
        })
    );
    // Repeating the same value replays the original decision, not the new
    // timestamp.
    assert_eq!(
        authority
            .ack(AckRequest {
                channel_id: head.channel_id,
                up_to_sequence: 1,
                acked_at_ms: 999,
            })
            .expect("ack replay"),
        AckDecision::Replayed(AckReceipt {
            channel_id: head.channel_id,
            consume_high_water: 1,
            acked_at_ms: 100,
        })
    );
    // Regression below the consume high-water fails closed.
    assert!(matches!(
        authority.ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 0,
            acked_at_ms: 101,
        }),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));
    // Ack beyond the durable maximum fails closed.
    assert!(matches!(
        authority.ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: 102,
        }),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));

    let empty = create(&authority, 1_024, 201);
    assert!(matches!(
        authority.ack(AckRequest {
            channel_id: empty.channel_id,
            up_to_sequence: 1,
            acked_at_ms: 103,
        }),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));
    // Acking zero on an untouched queue is the idempotent no-op boundary.
    assert!(matches!(
        authority.ack(AckRequest {
            channel_id: empty.channel_id,
            up_to_sequence: 0,
            acked_at_ms: 104,
        }),
        Ok(AckDecision::Replayed(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers clamping, idempotency, regression and bookkeeping reset.
fn compact_never_trims_unconsumed_prefix_and_is_idempotent() {
    let root = Root::new("compact");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    let first = enqueue(&authority, &head, 1, b"aaaa", 10);
    let second = enqueue(&authority, &head, 2, b"bbbbbb", 11);
    let third = enqueue(&authority, &head, 3, b"cc", 12);

    // Nothing is consumed: the request is clamped to the consume high-water
    // and deletes nothing.
    assert_eq!(
        authority
            .compact(head.channel_id, 3)
            .expect("compact unconsumed"),
        CompactDecision::Replayed(CompactReceipt {
            channel_id: head.channel_id,
            trim_high_water: 0,
        })
    );
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after refused compact"),
        vec![first.clone(), second.clone(), third.clone()]
    );

    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 2,
            acked_at_ms: 20,
        })
        .expect("ack prefix");
    assert_eq!(
        authority
            .compact(head.channel_id, 3)
            .expect("compact consumed prefix"),
        CompactDecision::Trimmed(CompactReceipt {
            channel_id: head.channel_id,
            trim_high_water: 2,
        })
    );
    // The consumed prefix rows are gone; the unconsumed tail remains.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after compact"),
        vec![third.clone()]
    );
    let state = authority
        .inspect_queue(head.channel_id)
        .expect("inspect after compact");
    assert_eq!(state.trim_high_water, 2);
    assert_eq!(state.consume_high_water, 2);
    // Byte bookkeeping is reset to exactly the live tail.
    assert_eq!(state.backlog_bytes, third.payload_bytes);
    assert_eq!(state.retained_bytes, third.payload_bytes);
    assert_eq!(state.backlog_bytes, 2);
    // Repeating the same effective watermark replays.
    assert_eq!(
        authority
            .compact(head.channel_id, 2)
            .expect("compact replay"),
        CompactDecision::Replayed(CompactReceipt {
            channel_id: head.channel_id,
            trim_high_water: 2,
        })
    );
    // Regression below the durable trim watermark fails closed.
    assert!(matches!(
        authority.compact(head.channel_id, 1),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full restart replay of the delivery lifecycle.
fn restart_replays_entries_cursors_bookkeeping_and_decisions() {
    let root = Root::new("restart");
    let (head, state_before, window_before) = {
        let authority = ChannelAuthority::open(root.path()).expect("open authority");
        let head = create(&authority, 128, 200);
        enqueue(&authority, &head, 11, b"alpha", 1_001);
        let second = enqueue(&authority, &head, 12, b"beta", 1_002);
        let third = enqueue(&authority, &head, 13, b"gamma", 1_003);
        authority
            .ack(AckRequest {
                channel_id: head.channel_id,
                up_to_sequence: 1,
                acked_at_ms: 1_100,
            })
            .expect("ack prefix");
        authority
            .compact(head.channel_id, 1)
            .expect("compact prefix");
        let fourth = enqueue(&authority, &head, 14, b"delta", 1_004);
        let state = authority
            .inspect_queue(head.channel_id)
            .expect("inspect before restart");
        assert_eq!(state.consume_high_water, 1);
        assert_eq!(state.trim_high_water, 1);
        assert_eq!(state.max_sequence, 4);
        assert_eq!(
            state.backlog_bytes,
            second.payload_bytes + third.payload_bytes + fourth.payload_bytes
        );
        let window = authority.receive(head.channel_id, 10).expect("receive");
        assert_eq!(window, vec![second, third, fourth]);
        (head, state, window)
    };

    let reopened = ChannelAuthority::open(root.path()).expect("reopen authority");
    // Entries, cursors and byte bookkeeping replay field-for-field.
    assert_eq!(
        reopened
            .inspect_queue(head.channel_id)
            .expect("inspect after restart"),
        state_before
    );
    assert_eq!(
        reopened
            .receive(head.channel_id, 10)
            .expect("receive after restart"),
        window_before
    );

    // Decision replays stay consistent after restart.
    assert_eq!(
        reopened
            .ack(AckRequest {
                channel_id: head.channel_id,
                up_to_sequence: 1,
                acked_at_ms: 9_999,
            })
            .expect("ack replay"),
        AckDecision::Replayed(AckReceipt {
            channel_id: head.channel_id,
            consume_high_water: 1,
            acked_at_ms: 1_100,
        })
    );
    assert_eq!(
        reopened
            .compact(head.channel_id, 1)
            .expect("compact replay"),
        CompactDecision::Replayed(CompactReceipt {
            channel_id: head.channel_id,
            trim_high_water: 1,
        })
    );
    match reopened
        .enqueue(request_for(&head, 14, b"delta", 1_004))
        .expect("enqueue replay")
    {
        EnqueueDecision::Replayed(entry) => assert_eq!(entry, window_before[2]),
        EnqueueDecision::Enqueued(_) => panic!("live-tail enqueue must replay"),
    }
    // A fresh enqueue continues the per-channel monotonic sequence.
    match reopened
        .enqueue(request_for(&head, 15, b"epsilon", 1_005))
        .expect("enqueue after restart")
    {
        EnqueueDecision::Enqueued(entry) => assert_eq!(entry.sequence, 5),
        EnqueueDecision::Replayed(_) => panic!("fresh enqueue cannot replay"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full cursor/bookkeeping tamper matrix.
fn tampered_queue_state_fails_closed_as_corrupt_record() {
    let root = Root::new("tamper");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    enqueue(&authority, &head, 1, b"one", 10);
    enqueue(&authority, &head, 2, b"two", 11);
    authority
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 1,
            acked_at_ms: 20,
        })
        .expect("ack prefix");

    let raw = Connection::open(root.db()).expect("open raw connection");
    let channel = head.channel_id.as_bytes().as_slice();

    // (a) consume high-water beyond the durable maximum.
    raw.execute(
        "UPDATE channel_queue_cursors SET consume_high_water=5 WHERE channel_id=?1",
        [channel],
    )
    .expect("tamper consume");
    assert!(matches!(
        authority.receive(head.channel_id, 10),
        Err(ChannelAuthorityError::CorruptRecord(_))
    ));
    assert!(matches!(
        authority.inspect_queue(head.channel_id),
        Err(ChannelAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE channel_queue_cursors SET consume_high_water=1 WHERE channel_id=?1",
        [channel],
    )
    .expect("restore consume");

    // (b) backlog bookkeeping drift.
    raw.execute(
        "UPDATE channel_queue_bytes SET backlog_bytes=backlog_bytes+1 WHERE channel_id=?1",
        [channel],
    )
    .expect("tamper backlog");
    assert!(matches!(
        authority.inspect_queue(head.channel_id),
        Err(ChannelAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE channel_queue_bytes SET backlog_bytes=backlog_bytes-1 WHERE channel_id=?1",
        [channel],
    )
    .expect("restore backlog");

    // (c) retained bookkeeping drift.
    raw.execute(
        "UPDATE channel_queue_bytes SET retained_bytes=retained_bytes+1 WHERE channel_id=?1",
        [channel],
    )
    .expect("tamper retained");
    assert!(matches!(
        authority.receive(head.channel_id, 10),
        Err(ChannelAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE channel_queue_bytes SET retained_bytes=retained_bytes-1 WHERE channel_id=?1",
        [channel],
    )
    .expect("restore retained");

    // (d) trim watermark pushed into the live prefix (residue).
    raw.execute(
        "UPDATE channel_queue_cursors SET trim_high_water=1 WHERE channel_id=?1",
        [channel],
    )
    .expect("tamper trim");
    assert!(matches!(
        authority.inspect_queue(head.channel_id),
        Err(ChannelAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE channel_queue_cursors SET trim_high_water=0 WHERE channel_id=?1",
        [channel],
    )
    .expect("restore trim");

    // The restored state serves reads again.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after restore")
            .len(),
        1
    );
}

#[test]
fn ddl_guards_reject_entry_mutation_outside_compaction() {
    let root = Root::new("ddl-guards");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1_024, 200);
    enqueue(&authority, &head, 1, b"guarded", 10);

    let raw = Connection::open(root.db()).expect("open raw connection");
    assert!(
        raw.execute(
            "UPDATE channel_queue_entries SET payload=x'00' WHERE channel_id=?1",
            [head.channel_id.as_bytes().as_slice()],
        )
        .is_err(),
        "queue entries are immutable"
    );
    let rejection = raw
        .execute(
            "DELETE FROM channel_queue_entries WHERE channel_id=?1",
            [head.channel_id.as_bytes().as_slice()],
        )
        .expect_err("live deletion must abort");
    let message = rejection.to_string();
    assert!(
        message.contains("compaction"),
        "unexpected rejection message: {message}"
    );
    assert!(
        raw.execute("DELETE FROM channel_queue_cursors", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM channel_queue_bytes", []).is_err());

    // The guarded queue still serves reads through the authority.
    assert_eq!(
        authority
            .receive(head.channel_id, 10)
            .expect("receive after guards")
            .len(),
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the v1->v2 migration and its backfill.
fn v1_database_migrates_to_v2_preserving_channel_data() {
    let root = Root::new("v1-migration");
    let head = {
        let authority = ChannelAuthority::open(root.path()).expect("open authority");
        let head = create(&authority, 64, 200);
        enqueue(&authority, &head, 1, b"legacy", 10);
        head
    };
    let raw = Connection::open(root.db()).expect("open raw connection");
    raw.execute_batch(
        "DROP TRIGGER channel_queue_entries_immutable_update;
         DROP TRIGGER channel_queue_entries_compaction_delete;
         DROP TRIGGER channel_queue_cursors_no_delete;
         DROP TRIGGER channel_queue_bytes_no_delete;
         DROP TABLE channel_queue_entries;
         DROP TABLE channel_queue_cursors;
         DROP TABLE channel_queue_bytes;
         PRAGMA user_version=1;",
    )
    .expect("roll back to v1");
    drop(raw);

    let migrated = ChannelAuthority::open(root.path()).expect("migrate to v2");
    assert_eq!(
        migrated
            .inspect_channel(head.channel_id)
            .expect("v1 channel data preserved"),
        head
    );
    {
        let raw = Connection::open(root.db()).expect("open raw connection");
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read version"),
            2
        );
    }

    // The migrated queue starts empty with zeroed cursors and bookkeeping.
    assert_eq!(
        migrated
            .inspect_queue(head.channel_id)
            .expect("inspect migrated queue"),
        QueueState {
            channel_id: head.channel_id,
            capacity_bytes: 64,
            consume_high_water: 0,
            trim_high_water: 0,
            backlog_bytes: 0,
            retained_bytes: 0,
            max_sequence: 0,
        }
    );
    assert!(
        migrated
            .receive(head.channel_id, 10)
            .expect("receive migrated queue")
            .is_empty()
    );
    match migrated
        .enqueue(request_for(&head, 2, b"post-migration", 20))
        .expect("enqueue on migrated queue")
    {
        EnqueueDecision::Enqueued(entry) => assert_eq!(entry.sequence, 1),
        EnqueueDecision::Replayed(_) => panic!("migrated queue cannot replay a fresh key"),
    }
}

#[test]
fn unknown_schema_version_fails_closed() {
    let root = Root::new("unknown-version");
    {
        let authority = ChannelAuthority::open(root.path()).expect("open authority");
        create(&authority, 32, 200);
    }
    let raw = Connection::open(root.db()).expect("open raw connection");
    raw.pragma_update(None, "user_version", 99)
        .expect("set unknown version");
    drop(raw);
    assert!(matches!(
        ChannelAuthority::open(root.path()),
        Err(ChannelAuthorityError::SchemaVersionUnsupported(99))
    ));
}
