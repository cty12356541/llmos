//! Payer metering ledger tests (the ADR-0007 payer-metering addendum,
//! increment 54): the `Attributed` accounting point inside the accepted
//! advance transaction, the `Unallocated` accounting point in the compact
//! transaction, the byte-exact reconciliation identity reported by
//! `inspect_attribution`, the frozen per-row policy version, the immutable
//! write-once rows, and the fail-closed tamper paths.
//!
//! Ledger rows are asserted through an out-of-band raw `SQLite` reader (the
//! same discipline as the fault-injection suite); the authority's own
//! observation entry is `inspect_attribution`.

use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest,
};
use nlos_topic::{
    ATTRIBUTION_POLICY_VERSION, AdvanceDecision, AdvanceReceipt, AdvanceRequest, AttributionReport,
    CreateTopicRequest, PatternSubscribeDecision, PublicationRecord, PublishDecision,
    PublishRequest, SubscribeDecision, SubscribePatternRequest, SubscribeRequest, SubscriberKey,
    SubscriptionRecord, TopicAuthority, TopicAuthorityError, TopicCompactDecision, TopicDecision,
    TopicId, TopicPolicy, TopicRecord, UnsubscribeDecision, UnsubscribeRequest,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn payer(seed: u8) -> ResourceAccountId {
    ResourceAccountId::from_bytes([seed; 16])
}

fn subscriber(seed: u8) -> SubscriberKey {
    SubscriberKey::from_bytes([seed; 16])
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
            "nlos-topic-attribution-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn topic_db(&self) -> PathBuf {
        self.0.join("topic-authority.db")
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    root: Root,
    channel: Arc<ChannelAuthority>,
    topics: TopicAuthority,
}

impl Harness {
    fn new(label: &str) -> Self {
        let root = Root::new(label);
        let channel =
            Arc::new(ChannelAuthority::open(root.0.clone()).expect("open channel authority"));
        let topics = TopicAuthority::open(root.0.clone(), Arc::clone(&channel))
            .expect("open topic authority");
        Self {
            root,
            channel,
            topics,
        }
    }
}

fn policy_for(max_recipients: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients,
        delivery_attempts: 3,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(7),
    }
}

fn create_channel(channel: &ChannelAuthority, capacity_bytes: u64, seed: u8) -> ChannelRecord {
    match channel
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

fn create_topic(
    topics: &TopicAuthority,
    channel_id: ChannelId,
    name: &[u8],
    policy: TopicPolicy,
    seed: u8,
) -> TopicRecord {
    match topics
        .create_topic(CreateTopicRequest {
            channel_id,
            name: name.to_vec(),
            policy,
            idempotency_key: key(seed),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    }
}

fn subscribe_at(
    topics: &TopicAuthority,
    topic_id: TopicId,
    seed: u8,
    at: u64,
) -> SubscriptionRecord {
    match topics.subscribe(SubscribeRequest {
        topic_id,
        subscriber_key: subscriber(seed),
        subscribed_at_ms: at,
    }) {
        Ok(SubscribeDecision::Subscribed(record)) => record,
        _ => panic!("fresh subscribe cannot fail or replay"),
    }
}

fn publish_at(
    topics: &TopicAuthority,
    topic_id: TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> PublicationRecord {
    match topics.publish(PublishRequest {
        topic_id,
        payload: payload.to_vec(),
        idempotency_key: key(seed),
        published_at_ms: at,
    }) {
        Ok(PublishDecision::Published(record)) => record,
        _ => panic!("fresh publish cannot fail or replay"),
    }
}

fn advance_subscription(
    topics: &TopicAuthority,
    subscription: &SubscriptionRecord,
    up_to: u64,
    at: u64,
) -> AdvanceReceipt {
    match topics.advance_with_token(
        AdvanceRequest {
            topic_id: subscription.topic_id,
            subscriber_key: subscription.subscriber_key,
            up_to_sequence: up_to,
            advanced_at_ms: at,
        },
        &subscription.consume_token,
    ) {
        Ok(AdvanceDecision::Advanced(receipt)) => receipt,
        _ => panic!("fresh advance cannot fail or replay"),
    }
}

fn unsubscribe(topics: &TopicAuthority, subscription: &SubscriptionRecord, at: u64) {
    match topics.unsubscribe_with_token(
        UnsubscribeRequest {
            topic_id: subscription.topic_id,
            subscriber_key: subscription.subscriber_key,
            unsubscribed_at_ms: at,
        },
        &subscription.consume_token,
    ) {
        Ok(UnsubscribeDecision::Unsubscribed(_)) => {}
        _ => panic!("active unsubscribe cannot fail or replay"),
    }
}

fn ack(channel: &ChannelAuthority, channel_id: ChannelId, up_to: u64, at: u64) {
    channel
        .ack(AckRequest {
            channel_id,
            up_to_sequence: up_to,
            acked_at_ms: at,
        })
        .expect("channel owner ack");
}

fn compact_trimmed(topics: &TopicAuthority, topic_id: TopicId, trim_to: u64) -> u64 {
    match topics.compact(topic_id, trim_to) {
        Ok(TopicCompactDecision::Trimmed(receipt)) => receipt.effective_trim_high_water,
        _ => panic!("expected a fresh trim"),
    }
}

/// One durable ledger row read out-of-band, every field the table carries.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LedgerRow {
    ledger_id: Vec<u8>,
    topic_id: Vec<u8>,
    payer: Vec<u8>,
    kind: i64,
    payload_bytes: i64,
    policy_version: i64,
    evidence_sequence: i64,
    recorded_at_ms: i64,
}

/// All ledger rows of the database, ordered by evidence sequence.
fn ledger_rows(database: &Path) -> Vec<LedgerRow> {
    let connection = Connection::open(database).expect("open raw reader");
    let mut statement = connection
        .prepare(
            "SELECT ledger_id, topic_id, payer_account_id, kind, payload_bytes,
                    policy_version, evidence_sequence, recorded_at_ms
             FROM topic_attribution_ledger ORDER BY evidence_sequence",
        )
        .expect("prepare ledger read");
    statement
        .query_map([], |row| {
            Ok(LedgerRow {
                ledger_id: row.get(0)?,
                topic_id: row.get(1)?,
                payer: row.get(2)?,
                kind: row.get(3)?,
                payload_bytes: row.get(4)?,
                policy_version: row.get(5)?,
                evidence_sequence: row.get(6)?,
                recorded_at_ms: row.get(7)?,
            })
        })
        .expect("query ledger rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("ledger rows readable")
}

fn ledger_count(database: &Path) -> usize {
    ledger_rows(database).len()
}

/// Runs raw SQL against the topic database (the tamper paths: the corruption
/// this simulates bypasses every authority invariant, so the test first
/// drops the write-once triggers where the tamper needs UPDATE or DELETE).
fn raw_exec(database: &Path, sql: &str) -> Result<(), String> {
    let connection = Connection::open(database).expect("open raw writer");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
        .execute_batch(sql)
        .map_err(|error| error.to_string())
}

fn drop_ledger_triggers(database: &Path) {
    raw_exec(
        database,
        "DROP TRIGGER topic_attribution_ledger_no_update;
         DROP TRIGGER topic_attribution_ledger_no_delete;",
    )
    .expect("drop the write-once triggers");
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn report(topics: &TopicAuthority, topic_id: TopicId) -> AttributionReport {
    topics
        .inspect_attribution(topic_id)
        .expect("attribution report")
}

const ATTRIBUTED: i64 = 1;
const UNALLOCATED: i64 = 2;

/// Single subscriber consumes everything: every publication is `Attributed`
/// exactly once with the publication payer, and the report is fully settled
/// and balanced (`attributed == total`, nothing unallocated or unsettled).
#[test]
fn full_consumption_attributes_every_publication_once() {
    let harness = Harness::new("full-consumption");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"metered",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    let first = publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    let second = publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    let third = publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_003);
    assert_eq!(
        [
            first.channel_sequence,
            second.channel_sequence,
            third.channel_sequence
        ],
        [1, 2, 3]
    );

    advance_subscription(&harness.topics, &a, 3, 8_100);

    let expected_payer = payer(7).as_bytes().to_vec();
    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 3, "one ledger row per crossed publication");
    for (row, (sequence, payload_bytes)) in rows.iter().zip([(1, 3), (2, 4), (3, 5)]) {
        assert_eq!(row.kind, ATTRIBUTED);
        assert_eq!(row.evidence_sequence, sequence);
        assert_eq!(row.payload_bytes, payload_bytes);
        assert_eq!(row.payer, expected_payer);
        assert_eq!(row.policy_version, 1);
        assert_eq!(row.recorded_at_ms, 8_100);
        assert_eq!(row.topic_id, topic.topic_id.as_bytes());
    }
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 12,
            unallocated_bytes: 0,
            unsettled_bytes: 0,
            total: 12,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// Several subscribers advance over overlapping windows: first-cross-wins —
/// each sequence is attributed once by the first advance crossing it, later
/// overlapping advances add nothing, and the advance replay path writes no
/// row at all.
#[test]
fn overlapping_advances_bill_each_sequence_exactly_once() {
    let harness = Harness::new("overlap");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"overlap",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    let b = subscribe_at(&harness.topics, topic.topic_id, 2, 8_001);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_002);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_003);
    publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_004);

    advance_subscription(&harness.topics, &a, 2, 8_100);
    advance_subscription(&harness.topics, &b, 3, 8_101);

    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 3, "sequences 1-2 by A's call, 3 by B's call");
    assert_eq!(rows[0].recorded_at_ms, 8_100);
    assert_eq!(rows[1].recorded_at_ms, 8_100);
    assert_eq!(rows[2].recorded_at_ms, 8_101);
    assert!(rows.iter().all(|row| row.kind == ATTRIBUTED));

    // Repeating A's exact cursor replays the stored decision and writes
    // nothing.
    match harness.topics.advance_with_token(
        AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: a.subscriber_key,
            up_to_sequence: 2,
            advanced_at_ms: 8_200,
        },
        &a.consume_token,
    ) {
        Ok(AdvanceDecision::Replayed(_)) => {}
        _ => panic!("identical cursor must replay"),
    }
    assert_eq!(ledger_count(&harness.root.topic_db()), 3);
    assert_eq!(report(&harness.topics, topic.topic_id).attributed_bytes, 12);
}

/// One advance jumping several sequences records the whole crossed window in
/// its single transaction: one row per publication, all stamped with the one
/// call's time.
#[test]
fn multi_sequence_jump_records_whole_window_in_one_advance() {
    let harness = Harness::new("jump");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"jump",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_003);

    let receipt = advance_subscription(&harness.topics, &a, 3, 8_200);
    assert_eq!(receipt.cursor, 3);

    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.recorded_at_ms == 8_200));
    assert_eq!(
        rows.iter().map(|row| row.payload_bytes).collect::<Vec<_>>(),
        [3, 4, 5]
    );
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 12,
            unallocated_bytes: 0,
            unsettled_bytes: 0,
            total: 12,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// A zero-window advance (repeating the current cursor) is a replay: no
/// transaction effect, zero ledger rows — before and after real advances.
#[test]
fn zero_window_advance_writes_no_ledger_rows() {
    let harness = Harness::new("zero-window");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"zero",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);

    match harness.topics.advance_with_token(
        AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: a.subscriber_key,
            up_to_sequence: 0,
            advanced_at_ms: 8_100,
        },
        &a.consume_token,
    ) {
        Ok(AdvanceDecision::Replayed(_)) => {}
        _ => panic!("zero-window advance must replay"),
    }
    assert_eq!(ledger_count(&harness.root.topic_db()), 0);

    advance_subscription(&harness.topics, &a, 1, 8_200);
    assert_eq!(ledger_count(&harness.root.topic_db()), 1);

    match harness.topics.advance_with_token(
        AdvanceRequest {
            topic_id: topic.topic_id,
            subscriber_key: a.subscriber_key,
            up_to_sequence: 1,
            advanced_at_ms: 8_300,
        },
        &a.consume_token,
    ) {
        Ok(AdvanceDecision::Replayed(_)) => {}
        _ => panic!("identical cursor must replay"),
    }
    assert_eq!(ledger_count(&harness.root.topic_db()), 1);
    assert_eq!(report(&harness.topics, topic.topic_id).attributed_bytes, 3);
}

/// Compact deletes an unconsumed prefix: the consumed publication stays
/// `Attributed`, the deleted-but-never-delivered ones become `Unallocated`
/// rows (publication payer, recorded at the `0` no-time marker), and the
/// report closes byte-exact.
#[test]
fn compact_records_unallocated_for_unconsumed_prefix() {
    let harness = Harness::new("unallocated");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"unalloc",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcde", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"vw", 8_003);

    advance_subscription(&harness.topics, &a, 1, 8_100);
    unsubscribe(&harness.topics, &a, 8_110);
    ack(&harness.channel, head.channel_id, 3, 8_120);
    assert_eq!(compact_trimmed(&harness.topics, topic.topic_id, 9), 3);

    let expected_payer = payer(7).as_bytes().to_vec();
    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].kind, ATTRIBUTED);
    assert_eq!(rows[0].payload_bytes, 3);
    assert_eq!(rows[0].recorded_at_ms, 8_100);
    assert_eq!(rows[1].kind, UNALLOCATED);
    assert_eq!(rows[1].payload_bytes, 5);
    assert_eq!(rows[1].recorded_at_ms, 0, "compact carries no caller time");
    assert_eq!(rows[2].kind, UNALLOCATED);
    assert_eq!(rows[2].payload_bytes, 2);
    assert!(rows.iter().all(|row| row.payer == expected_payer));
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 3,
            unallocated_bytes: 7,
            unsettled_bytes: 0,
            total: 10,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// An isolated (quarantined) subscriber's lag, trimmed away after it
/// unsubscribes, lands as `Unallocated` rows carrying the *publication*
/// payer — metering follows the delivery fact, never the subscriber.
#[test]
fn quarantined_lag_trimmed_as_unallocated_carries_publication_payer() {
    let harness = Harness::new("quarantined");
    let head = create_channel(&harness.channel, 1_024, 200);
    let mut isolated = policy_for(4);
    isolated.delivery_attempts = 1;
    let topic = create_topic(&harness.topics, head.channel_id, b"isolated", isolated, 101);
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    // Sequence 1 arrives while A is caught up (not billed); sequence 2
    // arrives while A lags: the one declared attempt is spent and A flips
    // quarantined with sequences 1-2 unconsumed.
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    assert!(matches!(
        harness
            .topics
            .poll(topic.topic_id, a.subscriber_key, 10)
            .err(),
        Some(TopicAuthorityError::DeliveryQuarantined(_))
    ));

    unsubscribe(&harness.topics, &a, 8_110);
    ack(&harness.channel, head.channel_id, 2, 8_120);
    assert_eq!(compact_trimmed(&harness.topics, topic.topic_id, 9), 2);

    let expected_payer = payer(7).as_bytes().to_vec();
    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 2, "the whole quarantined lag is unallocated");
    assert!(rows.iter().all(|row| row.kind == UNALLOCATED));
    assert!(rows.iter().all(|row| row.payer == expected_payer));
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 0,
            unallocated_bytes: 7,
            unsettled_bytes: 0,
            total: 7,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// Ledger rows are write-once: `UPDATE` and `DELETE` are rejected by the
/// triggers, one sequence can never carry a second row, and the policy
/// version is frozen at 1 inside every row.
#[test]
fn ledger_rows_are_immutable_and_one_per_sequence() {
    let harness = Harness::new("immutable");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"frozen",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    advance_subscription(&harness.topics, &a, 2, 8_100);

    let database = &harness.root.topic_db();
    assert_eq!(ledger_count(database), 2);
    let version = i64::try_from(ATTRIBUTION_POLICY_VERSION).expect("version fits i64");
    assert!(
        ledger_rows(database)
            .iter()
            .all(|row| row.policy_version == version)
    );

    let update_error = raw_exec(
        database,
        "UPDATE topic_attribution_ledger SET payload_bytes=99",
    )
    .unwrap_err();
    assert!(update_error.contains("immutable"), "{update_error}");
    let delete_error = raw_exec(database, "DELETE FROM topic_attribution_ledger").unwrap_err();
    assert!(delete_error.contains("durable"), "{delete_error}");
    assert_eq!(ledger_count(database), 2, "tamper attempts wrote nothing");

    // A second accounting event for one sequence is structurally impossible.
    let topic_hex = hex(topic.topic_id.as_bytes());
    let payer_hex = hex(payer(7).as_bytes());
    let padding = hex(&[0; 15]);
    let duplicate_error = raw_exec(
        database,
        &format!(
            "INSERT INTO topic_attribution_ledger (
                 ledger_id, topic_id, payer_account_id, kind, payload_bytes,
                 policy_version, evidence_sequence, recorded_at_ms
             ) VALUES (x'FF{padding}', x'{topic_hex}', x'{payer_hex}', 2, 3, 1, 1, 0)"
        ),
    )
    .unwrap_err();
    assert!(
        duplicate_error.contains("UNIQUE constraint failed"),
        "{duplicate_error}"
    );
}

/// Tampered ledger state fails closed at inspection time: bytes rewritten,
/// a row deleted under an advanced trim watermark, and a row bound to a
/// sequence with no publication are all `CorruptRecord`, never a report.
#[test]
fn inspect_fails_closed_on_tampered_ledger() {
    // Bytes rewritten under a dropped trigger: the row no longer matches its
    // publication's recorded payload length.
    let harness = Harness::new("tamper-bytes");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"tamper1",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    advance_subscription(&harness.topics, &a, 2, 8_100);
    drop_ledger_triggers(&harness.root.topic_db());
    raw_exec(
        &harness.root.topic_db(),
        "UPDATE topic_attribution_ledger SET payload_bytes=payload_bytes+1
         WHERE evidence_sequence=1",
    )
    .expect("tamper without triggers");
    assert!(matches!(
        harness.topics.inspect_attribution(topic.topic_id),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));

    // A row deleted for a publication the channel already trimmed: an
    // uncovered sequence at or below the trim watermark is a hole.
    let harness = Harness::new("tamper-delete");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"tamper2",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    advance_subscription(&harness.topics, &a, 2, 8_100);
    ack(&harness.channel, head.channel_id, 2, 8_110);
    assert_eq!(compact_trimmed(&harness.topics, topic.topic_id, 9), 2);
    drop_ledger_triggers(&harness.root.topic_db());
    raw_exec(
        &harness.root.topic_db(),
        "DELETE FROM topic_attribution_ledger WHERE evidence_sequence=1",
    )
    .expect("delete without triggers");
    assert!(matches!(
        harness.topics.inspect_attribution(topic.topic_id),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));

    // A ledger row bound to a sequence no publication ever enqueued.
    let harness = Harness::new("tamper-orphan");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"tamper3",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    advance_subscription(&harness.topics, &a, 1, 8_100);
    let topic_hex = hex(topic.topic_id.as_bytes());
    let payer_hex = hex(payer(7).as_bytes());
    let padding = hex(&[0; 15]);
    raw_exec(
        &harness.root.topic_db(),
        &format!(
            "INSERT INTO topic_attribution_ledger (
                 ledger_id, topic_id, payer_account_id, kind, payload_bytes,
                 policy_version, evidence_sequence, recorded_at_ms
             ) VALUES (x'EE{padding}', x'{topic_hex}', x'{payer_hex}', 1, 3, 1, 99, 0)"
        ),
    )
    .expect("insert a rogue row");
    assert!(matches!(
        harness.topics.inspect_attribution(topic.topic_id),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
}

/// A crash window between the channel trim and the ledger write is healed by
/// re-running the compact: the replaying pass re-records the missing
/// `Unallocated` rows (and records nothing new in a healthy replay), so the
/// report closes again.
#[test]
fn compact_replay_heals_missing_unallocated_rows() {
    let harness = Harness::new("heal");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"heal",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_003);
    unsubscribe(&harness.topics, &a, 8_110);
    ack(&harness.channel, head.channel_id, 3, 8_120);
    assert_eq!(compact_trimmed(&harness.topics, topic.topic_id, 9), 3);
    assert_eq!(ledger_count(&harness.root.topic_db()), 3);

    // Simulate the crash window: the channel prefix is gone but the ledger
    // transaction never landed.
    drop_ledger_triggers(&harness.root.topic_db());
    raw_exec(
        &harness.root.topic_db(),
        "DELETE FROM topic_attribution_ledger",
    )
    .expect("simulate the lost ledger transaction");
    assert_eq!(ledger_count(&harness.root.topic_db()), 0);

    match harness.topics.compact(topic.topic_id, 9) {
        Ok(TopicCompactDecision::Replayed(receipt)) => {
            assert_eq!(receipt.effective_trim_high_water, 3);
        }
        _ => panic!("identical watermark must replay"),
    }
    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row.kind == UNALLOCATED));
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 0,
            unallocated_bytes: 12,
            unsettled_bytes: 0,
            total: 12,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// Reopen replays the ledger byte-identically: every field of every row is
/// equal after a full close/reopen cycle, the report reconciles to the same
/// value, and the additive v7 migration left the `user_version` watermark
/// untouched.
#[test]
fn reopen_replays_ledger_field_equal_and_reconciles() {
    let harness = Harness::new("reopen");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"reopen",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_003);
    advance_subscription(&harness.topics, &a, 2, 8_100);
    unsubscribe(&harness.topics, &a, 8_110);
    ack(&harness.channel, head.channel_id, 3, 8_120);
    assert_eq!(compact_trimmed(&harness.topics, topic.topic_id, 9), 3);

    let before_rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(before_rows.len(), 3);
    let before_report = report(&harness.topics, topic.topic_id);
    assert_eq!(before_report.attributed_bytes, 7);
    assert_eq!(before_report.unallocated_bytes, 5);

    let version: i64 = {
        let connection = Connection::open(harness.root.topic_db()).expect("raw reader");
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    };
    assert_eq!(
        version, 5,
        "the additive v7 step must not bump the watermark"
    );

    drop(harness.topics);
    let reopened = TopicAuthority::open(harness.root.0.clone(), Arc::clone(&harness.channel))
        .expect("reopen topic authority");
    assert_eq!(ledger_rows(&harness.root.topic_db()), before_rows);
    assert_eq!(report(&reopened, topic.topic_id), before_report);
}

/// A pattern-attached subscription is an ordinary row: advancing it records
/// the same `Attributed` rows with the publication payer, and the report
/// balances — fanout metering is per publication, never per subscriber.
#[test]
fn pattern_attached_subscription_advance_accounts_the_same() {
    let harness = Harness::new("pattern");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"metered",
        policy_for(4),
        101,
    );
    let Ok(PatternSubscribeDecision::Subscribed(outcome)) =
        harness.topics.subscribe_pattern(SubscribePatternRequest {
            pattern: b"met*".to_vec(),
            binding: payer(9),
            subscriber_key: subscriber(5),
            idempotency_key: key(51),
            subscribed_at_ms: 8_010,
        })
    else {
        panic!("fresh pattern subscribe cannot fail or replay");
    };
    assert_eq!(outcome.report.attached.len(), 1);
    assert_eq!(outcome.report.attached[0].topic_id, topic.topic_id);
    let attached = outcome.report.attached[0].subscription;
    assert!(attached.attached_by.is_some());

    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_020);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_021);
    advance_subscription(&harness.topics, &attached, 2, 8_100);

    let expected_payer = payer(7).as_bytes().to_vec();
    let rows = ledger_rows(&harness.root.topic_db());
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.kind == ATTRIBUTED));
    assert!(rows.iter().all(|row| row.payer == expected_payer));
    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 7,
            unallocated_bytes: 0,
            unsettled_bytes: 0,
            total: 7,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
}

/// Live backlog is the third term of the identity: publications nobody has
/// crossed and no trim has deleted stay `unsettled`, so the identity closes
/// while the backlog is open — and a channel prefix compacted *bypassing*
/// the topic (deleted bytes no accounting event ever covered) fails closed
/// instead of silently vanishing from the meter.
#[test]
fn unsettled_backlog_balances_and_bypassed_trim_fails_closed() {
    let harness = Harness::new("unsettled");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"backlog",
        policy_for(4),
        101,
    );
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    advance_subscription(&harness.topics, &a, 1, 8_100);

    assert_eq!(
        report(&harness.topics, topic.topic_id),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 3,
            unallocated_bytes: 0,
            unsettled_bytes: 4,
            total: 7,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );

    // The channel owner trims past the unconsumed publication without going
    // through the topic service: from the metering authority's point of view
    // the bytes are deleted with no accounting event, which is corruption.
    ack(&harness.channel, head.channel_id, 2, 8_200);
    let _bypassed = harness
        .channel
        .compact(head.channel_id, 2)
        .expect("bypassing channel compact");
    assert!(matches!(
        harness.topics.inspect_attribution(topic.topic_id),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
}
