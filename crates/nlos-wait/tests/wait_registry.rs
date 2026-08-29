use nlos_channel::{
    ChannelAuthority, ChannelAuthorityError, ChannelDecision, ChannelRecord, CreateChannelRequest,
};
use nlos_types::{ChannelId, IdempotencyKey};
use nlos_wait::{
    BindingId, CancelDecision, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision,
    RegisterWaitRequest, WaitAuthority, WaitAuthorityError, WaitId, WaitRecord, WaitState,
    WakeReport,
};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
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
            "nlos-wait-registry-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> PathBuf {
        self.0.join("wait-authority.db")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Pair {
    channel: Arc<ChannelAuthority>,
    wait: WaitAuthority,
}

fn open_pair(root: &Root) -> Pair {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait authority");
    Pair { channel, wait }
}

fn create_channel(authority: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn register_request(
    channel: &ChannelRecord,
    waiter: u8,
    target_sequence: u64,
    key_seed: u8,
) -> RegisterWaitRequest {
    RegisterWaitRequest {
        binding: binding(waiter),
        channel_id: channel.channel_id,
        target_sequence,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_000,
    }
}

fn register(
    authority: &WaitAuthority,
    channel: &ChannelRecord,
    waiter: u8,
    target_sequence: u64,
    key_seed: u8,
) -> WaitRecord {
    match authority
        .register_wait(register_request(channel, waiter, target_sequence, key_seed))
        .expect("register wait")
    {
        RegisterDecision::Registered(record) => record,
        RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
    }
}

fn notify(
    authority: &WaitAuthority,
    channel_id: ChannelId,
    up_to_sequence: u64,
    key_seed: u8,
) -> WakeReport {
    authority
        .notify_commits(NotifyCommitsRequest {
            channel_id,
            up_to_sequence,
            notified_at_ms: 500,
            idempotency_key: key(key_seed),
        })
        .expect("notify commits")
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers replay, every drift axis and key independence.
fn register_replays_and_conflicts_on_drift() {
    let root = Root::new("register-replay");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let original = register(&pair.wait, &channel, 1, 5, 1);
    assert_eq!(original.state, WaitState::Pending);
    assert_eq!(original.binding, binding(1));
    assert_eq!(original.channel_id, channel.channel_id);
    assert_eq!(original.channel_generation, channel.generation);
    assert_eq!(original.channel_fencing_token, channel.fencing_token);
    assert_eq!(original.target_sequence, 5);
    assert_eq!(original.registered_at_ms, 1_000);
    assert_eq!(
        (original.woken_at_ms, original.woken_up_to_sequence),
        (0, 0)
    );
    assert_eq!(original.cancelled_at_ms, 0);

    // The exact request replays the original row; the registration
    // timestamp is authority state and is not compared on replay.
    let mut replayed_request = register_request(&channel, 1, 5, 1);
    match pair
        .wait
        .register_wait(replayed_request)
        .expect("exact replay")
    {
        RegisterDecision::Replayed(record) => assert_eq!(record, original),
        RegisterDecision::Registered(_) => panic!("exact replay must not register again"),
    }
    replayed_request.registered_at_ms = 9_999;
    match pair
        .wait
        .register_wait(replayed_request)
        .expect("replay ignores the timestamp")
    {
        RegisterDecision::Replayed(record) => {
            assert_eq!(record.registered_at_ms, 1_000);
        }
        RegisterDecision::Registered(_) => panic!("timestamp drift is not a rebinding"),
    }

    // The same key rebound to any drifted input is a conflict.
    replayed_request.registered_at_ms = 1_000;
    replayed_request.target_sequence = 6;
    assert!(matches!(
        pair.wait.register_wait(replayed_request),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));
    replayed_request.target_sequence = 5;
    replayed_request.binding = binding(2);
    assert!(matches!(
        pair.wait.register_wait(replayed_request),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));
    replayed_request.binding = binding(1);
    // Channel drift is exercised against a second real channel: an unknown
    // channel would fail closed earlier, at the owner readback gate.
    let other = create_channel(&pair.channel, 201);
    replayed_request.channel_id = other.channel_id;
    assert!(matches!(
        pair.wait.register_wait(replayed_request),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));

    // A fresh key with identical fields is a distinct wait.
    let sibling = register(&pair.wait, &channel, 1, 5, 2);
    assert_ne!(sibling.wait_id, original.wait_id);
    let listed = pair
        .wait
        .inspect_channel_waits(channel.channel_id)
        .expect("list waits");
    assert_eq!(listed.len(), 2);
}

#[test]
fn register_validation_rejects_before_any_durable_write() {
    let root = Root::new("register-validation");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);

    // target_sequence 0 is rejected pre-write.
    assert!(matches!(
        pair.wait.register_wait(register_request(&channel, 1, 0, 9)),
        Err(WaitAuthorityError::InvalidSequence(_))
    ));
    // The all-zero binding is rejected pre-write.
    assert!(matches!(
        pair.wait.register_wait(register_request(&channel, 0, 3, 9)),
        Err(WaitAuthorityError::InvalidBinding)
    ));
    // An unknown Channel fails closed through the owner readback.
    assert!(matches!(
        pair.wait.register_wait(RegisterWaitRequest {
            binding: binding(1),
            channel_id: ChannelId::from_bytes([0x55; 16]),
            target_sequence: 3,
            idempotency_key: key(9),
            registered_at_ms: 1_000,
        }),
        Err(WaitAuthorityError::Channel(
            ChannelAuthorityError::ChannelNotFound(_)
        ))
    ));

    // Every rejection left zero durable state: the channel has no waits and
    // the rejected idempotency key is still free.
    assert!(
        pair.wait
            .inspect_channel_waits(channel.channel_id)
            .expect("list after rejections")
            .is_empty()
    );
    match pair
        .wait
        .register_wait(register_request(&channel, 1, 3, 9))
        .expect("retry after rejections")
    {
        RegisterDecision::Registered(_) => {}
        RegisterDecision::Replayed(_) => panic!("a rejection must not leave a replay record"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the empty, unknown and zero boundaries of notify.
fn notify_empty_report_unknown_channel_and_zero_up_to() {
    let root = Root::new("notify-boundaries");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);

    // A channel without waits reports an empty wake set, twice, under
    // distinct keys: an empty report is a success, not an error.
    let first = notify(&pair.wait, channel.channel_id, 5, 30);
    assert!(first.woken.is_empty());
    let second = notify(&pair.wait, channel.channel_id, 5, 31);
    assert!(second.woken.is_empty());

    // An unknown channel fails closed through the owner readback.
    assert!(matches!(
        pair.wait.notify_commits(NotifyCommitsRequest {
            channel_id: ChannelId::from_bytes([0x66; 16]),
            up_to_sequence: 5,
            notified_at_ms: 500,
            idempotency_key: key(32),
        }),
        Err(WaitAuthorityError::Channel(
            ChannelAuthorityError::ChannelNotFound(_)
        ))
    ));
    // up_to_sequence 0 is rejected before any write.
    assert!(matches!(
        pair.wait.notify_commits(NotifyCommitsRequest {
            channel_id: channel.channel_id,
            up_to_sequence: 0,
            notified_at_ms: 500,
            idempotency_key: key(33),
        }),
        Err(WaitAuthorityError::InvalidSequence(_))
    ));
    // A zero notification timestamp would collide with the durable "not
    // woken" sentinel and is rejected before any write.
    assert!(matches!(
        pair.wait.notify_commits(NotifyCommitsRequest {
            channel_id: channel.channel_id,
            up_to_sequence: 5,
            notified_at_ms: 0,
            idempotency_key: key(34),
        }),
        Err(WaitAuthorityError::InvalidTimestamp(_))
    ));
}

#[test]
fn notify_wakes_exact_pending_subset_including_boundary() {
    let root = Root::new("notify-subset");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let at_three = register(&pair.wait, &channel, 11, 3, 1);
    let at_four = register(&pair.wait, &channel, 12, 4, 2);
    let at_five = register(&pair.wait, &channel, 13, 5, 3);

    let report = notify(&pair.wait, channel.channel_id, 4, 30);
    assert_eq!(report.woken.len(), 2);
    assert_eq!(report.woken[0], {
        let mut woken = at_three;
        woken.state = WaitState::Woken;
        woken.woken_at_ms = 500;
        woken.woken_up_to_sequence = 4;
        woken
    });
    assert_eq!(report.woken[1].wait_id, at_four.wait_id);
    // The boundary is inclusive and the wait above it stays pending.
    assert_eq!(
        pair.wait
            .inspect_wait(at_five.wait_id)
            .expect("inspect unnotified wait")
            .state,
        WaitState::Pending
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers replay-no-reflip plus the remainder wake.
fn notify_replay_does_not_double_flip_and_later_notify_wakes_remainder() {
    let root = Root::new("notify-replay");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let early = register(&pair.wait, &channel, 11, 2, 1);
    let late = register(&pair.wait, &channel, 12, 4, 2);

    let original = notify(&pair.wait, channel.channel_id, 4, 30);
    assert_eq!(original.woken.len(), 2);

    // The exact key replays the recorded original report (even with a
    // drifted notification timestamp, which is authority state).
    let mut replay = NotifyCommitsRequest {
        channel_id: channel.channel_id,
        up_to_sequence: 4,
        notified_at_ms: 500,
        idempotency_key: key(30),
    };
    assert_eq!(
        pair.wait
            .notify_commits(replay)
            .expect("exact notify replay"),
        original
    );
    replay.notified_at_ms = 9_999;
    let WakeReport { woken } = pair.wait.notify_commits(replay).expect("notify replay");
    assert_eq!(woken.len(), 2);
    assert!(woken.iter().all(|record| record.woken_at_ms == 500));

    // A different key over the same range flips nothing: already-WOKEN rows
    // are terminal and their wake fields are untouched.
    let again = notify(&pair.wait, channel.channel_id, 4, 31);
    assert!(again.woken.is_empty());
    assert_eq!(
        pair.wait
            .inspect_wait(early.wait_id)
            .expect("inspect after re-notify")
            .woken_at_ms,
        500
    );

    // A later, higher notification wakes only the remainder.
    let tail = register(&pair.wait, &channel, 13, 6, 3);
    let remainder = notify(&pair.wait, channel.channel_id, 6, 32);
    assert_eq!(remainder.woken.len(), 1);
    assert_eq!(remainder.woken[0].wait_id, tail.wait_id);
    assert_eq!(
        pair.wait
            .inspect_wait(late.wait_id)
            .expect("inspect early wait")
            .woken_up_to_sequence,
        4
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the cancel state machine and its receipts.
fn cancel_pending_succeeds_replays_and_fails_closed_on_terminal_states() {
    let root = Root::new("cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let first = register(&pair.wait, &channel, 11, 2, 1);
    let second = register(&pair.wait, &channel, 12, 3, 2);
    let third = register(&pair.wait, &channel, 13, 4, 3);

    let cancelled = match pair
        .wait
        .cancel_wait(CancelWaitRequest {
            wait_id: first.wait_id,
            cancelled_at_ms: 700,
            idempotency_key: key(40),
        })
        .expect("cancel pending wait")
    {
        CancelDecision::Cancelled(record) => record,
        CancelDecision::Replayed(_) => panic!("fresh cancel cannot replay"),
    };
    assert_eq!(cancelled.state, WaitState::Cancelled);
    assert_eq!(cancelled.cancelled_at_ms, 700);
    assert_eq!(cancelled.woken_at_ms, 0);

    // The exact key replays the original cancellation, keeping its stored
    // timestamp even when the request presents a new one.
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: first.wait_id,
            cancelled_at_ms: 9_999,
            idempotency_key: key(40),
        }),
        Ok(CancelDecision::Replayed(record))
            if record.cancelled_at_ms == 700
    ));
    // A fresh key against the terminal row fails closed.
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: first.wait_id,
            cancelled_at_ms: 701,
            idempotency_key: key(41),
        }),
        Err(WaitAuthorityError::WaitNotPending(WaitState::Cancelled))
    ));

    // The notify skips the cancelled wait and wakes only pending ones.
    let report = notify(&pair.wait, channel.channel_id, 4, 30);
    assert_eq!(
        report
            .woken
            .iter()
            .map(|record| record.wait_id)
            .collect::<Vec<_>>(),
        vec![second.wait_id, third.wait_id]
    );
    // A woken wait can never be retroactively cancelled.
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: second.wait_id,
            cancelled_at_ms: 702,
            idempotency_key: key(42),
        }),
        Err(WaitAuthorityError::WaitNotPending(WaitState::Woken))
    ));
    // Unknown waits and zero timestamps fail closed.
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: WaitId::from_bytes([0x78; 16]),
            cancelled_at_ms: 703,
            idempotency_key: key(43),
        }),
        Err(WaitAuthorityError::WaitNotFound(_))
    ));
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: third.wait_id,
            cancelled_at_ms: 0,
            idempotency_key: key(44),
        }),
        Err(WaitAuthorityError::InvalidTimestamp(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers cross-channel isolation of wake fanout.
fn mixed_channel_notify_wakes_only_matching_channel() {
    let root = Root::new("mixed-channels");
    let pair = open_pair(&root);
    let alpha = create_channel(&pair.channel, 200);
    let beta = create_channel(&pair.channel, 201);
    let alpha_one = register(&pair.wait, &alpha, 11, 1, 1);
    let alpha_two = register(&pair.wait, &alpha, 12, 2, 2);
    let beta_one = register(&pair.wait, &beta, 13, 1, 3);
    let beta_two = register(&pair.wait, &beta, 14, 2, 4);

    let report = notify(&pair.wait, alpha.channel_id, 2, 30);
    assert_eq!(
        report
            .woken
            .iter()
            .map(|record| record.wait_id)
            .collect::<Vec<_>>(),
        vec![alpha_one.wait_id, alpha_two.wait_id]
    );
    // Channel beta is untouched, including its identical target sequences.
    let beta_waits = pair
        .wait
        .inspect_channel_waits(beta.channel_id)
        .expect("inspect beta waits");
    assert!(
        beta_waits
            .iter()
            .all(|record| record.state == WaitState::Pending)
    );

    // A beta notification wakes only beta's covered prefix.
    let beta_report = notify(&pair.wait, beta.channel_id, 1, 31);
    assert_eq!(beta_report.woken.len(), 1);
    assert_eq!(beta_report.woken[0].wait_id, beta_one.wait_id);
    assert_eq!(
        pair.wait
            .inspect_wait(beta_two.wait_id)
            .expect("inspect beta tail")
            .state,
        WaitState::Pending
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full restart replay and post-restart wake.
fn restart_replays_wait_rows_and_post_restart_notify_still_wakes() {
    let root = Root::new("restart");
    let (alpha_waits_before, beta_waits_before, original_report) = {
        let pair = open_pair(&root);
        let alpha = create_channel(&pair.channel, 200);
        let beta = create_channel(&pair.channel, 201);
        let cancelled_one = register(&pair.wait, &alpha, 11, 3, 1);
        register(&pair.wait, &alpha, 12, 5, 2);
        // This one stays PENDING across the restart.
        register(&pair.wait, &alpha, 14, 9, 4);
        register(&pair.wait, &beta, 13, 2, 3);
        pair.wait
            .cancel_wait(CancelWaitRequest {
                wait_id: cancelled_one.wait_id,
                cancelled_at_ms: 700,
                idempotency_key: key(40),
            })
            .expect("cancel before restart");
        let report = notify(&pair.wait, alpha.channel_id, 5, 30);
        assert_eq!(report.woken.len(), 1);
        (
            pair.wait
                .inspect_channel_waits(alpha.channel_id)
                .expect("inspect alpha"),
            pair.wait
                .inspect_channel_waits(beta.channel_id)
                .expect("inspect beta"),
            report,
        )
    };

    let pair = open_pair(&root);
    let alpha_id = alpha_waits_before[0].channel_id;
    let beta_id = beta_waits_before[0].channel_id;
    // Every wait row replays field-for-field.
    assert_eq!(
        pair.wait
            .inspect_channel_waits(alpha_id)
            .expect("inspect alpha after restart"),
        alpha_waits_before
    );
    assert_eq!(
        pair.wait
            .inspect_channel_waits(beta_id)
            .expect("inspect beta after restart"),
        beta_waits_before
    );
    let pending_after_restart = alpha_waits_before
        .iter()
        .find(|record| record.state == WaitState::Pending)
        .expect("a pending wait survives the restart");

    // Decision replays stay consistent after restart.
    assert!(matches!(
        pair.wait.register_wait(RegisterWaitRequest {
            binding: alpha_waits_before[0].binding,
            channel_id: alpha_id,
            target_sequence: alpha_waits_before[0].target_sequence,
            idempotency_key: alpha_waits_before[0].idempotency_key,
            registered_at_ms: 9_999,
        }),
        Ok(RegisterDecision::Replayed(_))
    ));
    assert_eq!(
        pair.wait
            .notify_commits(NotifyCommitsRequest {
                channel_id: alpha_id,
                up_to_sequence: 5,
                notified_at_ms: 9_999,
                idempotency_key: key(30),
            })
            .expect("notify replay after restart"),
        original_report
    );
    assert!(matches!(
        pair.wait.cancel_wait(CancelWaitRequest {
            wait_id: alpha_waits_before[0].wait_id,
            cancelled_at_ms: 9_999,
            idempotency_key: key(40),
        }),
        Ok(CancelDecision::Replayed(record))
            if record.cancelled_at_ms == 700
    ));

    // A PENDING row registered before the restart is still wakeable.
    let report = notify(&pair.wait, alpha_id, 9, 31);
    assert_eq!(report.woken.len(), 1);
    assert_eq!(report.woken[0].wait_id, pending_after_restart.wait_id);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the tamper matrix across all guarded fields.
fn tampered_wait_rows_fail_closed_as_corrupt_record() {
    let root = Root::new("tamper");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let record = register(&pair.wait, &channel, 11, 3, 1);
    let wait_id = record.wait_id;
    let raw = Connection::open(root.db()).expect("open raw connection");

    // (a) An illegal status enum value is rejected by the schema...
    assert!(
        raw.execute(
            "UPDATE waits SET status=7 WHERE wait_id=?1",
            [wait_id.as_bytes().as_slice()]
        )
        .is_err(),
        "status enum is schema-guarded"
    );
    // ...and a state-field drift that passes the schema (wake fields on a
    // pending row) fails closed at the authority readback.
    raw.execute(
        "UPDATE waits SET woken_at_ms=500 WHERE wait_id=?1",
        [wait_id.as_bytes().as_slice()],
    )
    .expect("tamper wake field");
    assert!(matches!(
        pair.wait.inspect_wait(wait_id),
        Err(WaitAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE waits SET woken_at_ms=0 WHERE wait_id=?1",
        [wait_id.as_bytes().as_slice()],
    )
    .expect("restore wake field");

    // (b) Channel binding drift: the snapshot generation is writable but the
    // binding digest no longer matches, so the readback fails closed.
    raw.execute(
        "UPDATE waits SET channel_generation=9 WHERE wait_id=?1",
        [wait_id.as_bytes().as_slice()],
    )
    .expect("tamper snapshot generation");
    assert!(matches!(
        pair.wait.inspect_wait(wait_id),
        Err(WaitAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE waits SET channel_generation=?1 WHERE wait_id=?2",
        params![
            i64::try_from(channel.generation.get()).expect("generation fits i64"),
            wait_id.as_bytes().as_slice()
        ],
    )
    .expect("restore snapshot generation");
    // A drifted channel id fails closed the same way...
    raw.execute(
        "UPDATE waits SET channel_id=?1 WHERE wait_id=?2",
        params![
            ChannelId::from_bytes([0x66; 16]).as_bytes().as_slice(),
            wait_id.as_bytes().as_slice()
        ],
    )
    .expect("tamper channel id");
    assert!(matches!(
        pair.wait.inspect_wait(wait_id),
        Err(WaitAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE waits SET channel_id=?1 WHERE wait_id=?2",
        params![
            channel.channel_id.as_bytes().as_slice(),
            wait_id.as_bytes().as_slice()
        ],
    )
    .expect("restore channel id");
    // ...while the registration identity itself is frozen by trigger.
    assert!(
        raw.execute(
            "UPDATE waits SET binding_id=?1 WHERE wait_id=?2",
            params![
                binding(12).as_bytes().as_slice(),
                wait_id.as_bytes().as_slice()
            ],
        )
        .is_err(),
        "wait binding is trigger-frozen"
    );

    // (c) target_sequence 0 is rejected by the schema; a drifted non-zero
    // target fails closed through the binding digest.
    assert!(
        raw.execute(
            "UPDATE waits SET target_sequence=0 WHERE wait_id=?1",
            [wait_id.as_bytes().as_slice()],
        )
        .is_err(),
        "target sequence zero is schema-guarded"
    );
    raw.execute(
        "UPDATE waits SET target_sequence=9 WHERE wait_id=?1",
        [wait_id.as_bytes().as_slice()],
    )
    .expect("tamper target sequence");
    assert!(matches!(
        pair.wait.inspect_wait(wait_id),
        Err(WaitAuthorityError::CorruptRecord(_))
    ));
    assert!(matches!(
        pair.wait.inspect_channel_waits(channel.channel_id),
        Err(WaitAuthorityError::CorruptRecord(_))
    ));
    raw.execute(
        "UPDATE waits SET target_sequence=?1 WHERE wait_id=?2",
        params![3, wait_id.as_bytes().as_slice()],
    )
    .expect("restore target sequence");

    // The restored row serves reads and wakes again.
    assert_eq!(
        pair.wait.inspect_wait(wait_id).expect("inspect restored"),
        record
    );
    assert_eq!(notify(&pair.wait, channel.channel_id, 3, 30).woken.len(), 1);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers listing vs single consistency across channels.
fn inspect_listing_matches_single_inspect() {
    let root = Root::new("inspect");
    let pair = open_pair(&root);
    let alpha = create_channel(&pair.channel, 200);
    let beta = create_channel(&pair.channel, 201);
    let alpha_one = register(&pair.wait, &alpha, 11, 1, 1);
    let alpha_two = register(&pair.wait, &alpha, 12, 2, 2);
    // Not covered by the notification below.
    let alpha_three = register(&pair.wait, &alpha, 13, 9, 4);
    let beta_one = register(&pair.wait, &beta, 14, 1, 3);
    notify(&pair.wait, alpha.channel_id, 2, 30);
    pair.wait
        .cancel_wait(CancelWaitRequest {
            wait_id: beta_one.wait_id,
            cancelled_at_ms: 700,
            idempotency_key: key(40),
        })
        .expect("cancel beta wait");

    for channel_id in [alpha.channel_id, beta.channel_id] {
        let listed = pair
            .wait
            .inspect_channel_waits(channel_id)
            .expect("list waits");
        assert!(!listed.is_empty());
        for record in listed {
            assert_eq!(
                pair.wait.inspect_wait(record.wait_id).expect("inspect one"),
                record
            );
        }
    }
    // Terminal states replay consistently across both read paths.
    assert_eq!(
        pair.wait
            .inspect_wait(alpha_one.wait_id)
            .expect("woken")
            .state,
        WaitState::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(alpha_two.wait_id)
            .expect("woken")
            .state,
        WaitState::Woken
    );
    assert_eq!(
        pair.wait
            .inspect_wait(beta_one.wait_id)
            .expect("cancelled")
            .state,
        WaitState::Cancelled
    );
    assert_eq!(
        pair.wait
            .inspect_wait(alpha_three.wait_id)
            .expect("pending")
            .state,
        WaitState::Pending
    );

    // An empty channel lists empty; unknown waits fail closed.
    let gamma = create_channel(&pair.channel, 202);
    assert!(
        pair.wait
            .inspect_channel_waits(gamma.channel_id)
            .expect("list empty channel")
            .is_empty()
    );
    assert!(matches!(
        pair.wait.inspect_wait(WaitId::from_bytes([0x79; 16])),
        Err(WaitAuthorityError::WaitNotFound(_))
    ));
    // Identities are distinct across channels even for identical targets.
    assert_ne!(alpha_one.wait_id, beta_one.wait_id);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the DDL guard matrix over all three tables.
fn ddl_guards_reject_illegal_wait_mutations() {
    let root = Root::new("ddl-guards");
    let pair = open_pair(&root);
    let channel = create_channel(&pair.channel, 200);
    let record = register(&pair.wait, &channel, 11, 2, 1);
    // Cancel first, while the wait is still PENDING; the later notification
    // then wakes nothing but still records its durable receipt.
    pair.wait
        .cancel_wait(CancelWaitRequest {
            wait_id: record.wait_id,
            cancelled_at_ms: 700,
            idempotency_key: key(40),
        })
        .expect("cancel for guards");
    assert!(
        notify(&pair.wait, channel.channel_id, 5, 30)
            .woken
            .is_empty()
    );
    let raw = Connection::open(root.db()).expect("open raw connection");

    let rejection = raw
        .execute(
            "DELETE FROM waits WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()],
        )
        .expect_err("wait deletion must abort");
    assert!(
        rejection.to_string().contains("durable"),
        "unexpected rejection message: {rejection}"
    );
    // Terminal rows can never transition again.
    assert!(
        raw.execute(
            "UPDATE waits SET status=0 WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    // Registration identity fields are frozen.
    assert!(
        raw.execute(
            "UPDATE waits SET registered_at_ms=1 WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()],
        )
        .is_err()
    );

    // Notify and cancellation receipts are immutable and durable.
    assert!(
        raw.execute("UPDATE channel_notifies SET up_to_sequence=9", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM channel_notifies", []).is_err());
    assert!(
        raw.execute("UPDATE wait_cancellations SET cancelled_at_ms=9", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM wait_cancellations", []).is_err());

    // The guarded registry still serves reads through the authority.
    assert_eq!(
        pair.wait
            .inspect_wait(record.wait_id)
            .expect("inspect after guards")
            .state,
        WaitState::Cancelled
    );
}

#[test]
fn partial_schema_fails_closed() {
    let root = Root::new("partial-schema");
    let channel = ChannelAuthority::open(root.path()).expect("open channel authority");
    let raw = Connection::open(root.db()).expect("open raw connection");
    raw.execute("CREATE TABLE waits (wait_id BLOB PRIMARY KEY)", [])
        .expect("create partial schema");
    drop(raw);
    assert!(matches!(
        WaitAuthority::open(root.path(), Arc::new(channel)),
        Err(WaitAuthorityError::CorruptRecord(
            "partial wait authority schema"
        ))
    ));
}

#[test]
fn unknown_schema_version_fails_closed() {
    let root = Root::new("unknown-version");
    {
        let pair = open_pair(&root);
        let channel = create_channel(&pair.channel, 200);
        register(&pair.wait, &channel, 11, 2, 1);
    }
    let raw = Connection::open(root.db()).expect("open raw connection");
    raw.pragma_update(None, "user_version", 99)
        .expect("set unknown version");
    drop(raw);
    let channel = ChannelAuthority::open(root.path()).expect("open channel authority");
    assert!(matches!(
        WaitAuthority::open(root.path(), Arc::new(channel)),
        Err(WaitAuthorityError::SchemaVersionUnsupported(99))
    ));
}
