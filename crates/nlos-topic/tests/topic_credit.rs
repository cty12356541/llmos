//! Publisher credit admission tests (the B-TOPIC-001 credit increment):
//! the durable prepaid account (`open_credit`), the payload-byte charge on
//! the publish/republish admission chain with the typed insufficient
//! rejection (zero partial state), the key-idempotent recharge, the
//! verified balance readback, the resumed-`PENDING_ENQUEUE` replay that
//! never double-charges, the reopen durability, and the strict separation
//! from the payer attribution ledger.
//!
//! Account rows and movement entries are asserted through an out-of-band
//! raw `SQLite` reader (the same discipline as the attribution suite); the
//! authority's own observation entry is `inspect_credit`.

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_topic::{
    AdvanceDecision, AdvanceRequest, AttributionReport, CreateTopicRequest, CreditAccountRecord,
    OpenCreditDecision, OpenCreditRequest, PublicationRecord, PublishDecision, PublishRequest,
    RechargeCreditDecision, RechargeCreditRequest, SubscribeDecision, SubscribeRequest,
    SubscriberKey, SubscriptionRecord, TopicAuthority, TopicAuthorityError, TopicDecision, TopicId,
    TopicPolicy, TopicRecord,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
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
            "nlos-topic-credit-{label}-{}-{nonce}-{sequence}",
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

fn policy_for(payer: ResourceAccountId) -> TopicPolicy {
    TopicPolicy {
        max_recipients: 4,
        delivery_attempts: 3,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer,
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

fn open_credit(
    topics: &TopicAuthority,
    payer: ResourceAccountId,
    initial_units: u64,
    seed: u8,
    at: u64,
) -> CreditAccountRecord {
    match topics.open_credit(OpenCreditRequest {
        payer,
        initial_units,
        idempotency_key: key(seed),
        opened_at_ms: at,
    }) {
        Ok(OpenCreditDecision::Opened(record)) => record,
        _ => panic!("fresh open cannot fail or replay"),
    }
}

fn recharge(
    topics: &TopicAuthority,
    payer: ResourceAccountId,
    units: u64,
    seed: u8,
    at: u64,
) -> CreditAccountRecord {
    match topics.recharge_credit(RechargeCreditRequest {
        payer,
        units,
        idempotency_key: key(seed),
        recharged_at_ms: at,
    }) {
        Ok(RechargeCreditDecision::Recharged(record)) => record,
        _ => panic!("fresh recharge cannot fail or replay"),
    }
}

fn balance(topics: &TopicAuthority, payer: ResourceAccountId) -> CreditAccountRecord {
    topics.inspect_credit(payer).expect("credit account")
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

fn advance_subscription(
    topics: &TopicAuthority,
    subscription: &SubscriptionRecord,
    up_to: u64,
    at: u64,
) {
    match topics.advance_with_token(
        AdvanceRequest {
            topic_id: subscription.topic_id,
            subscriber_key: subscription.subscriber_key,
            up_to_sequence: up_to,
            advanced_at_ms: at,
        },
        &subscription.consume_token,
    ) {
        Ok(AdvanceDecision::Advanced(_)) => {}
        _ => panic!("fresh advance cannot fail or replay"),
    }
}

/// Runs raw SQL against the topic database (the tamper and crash-window
/// simulations: the state this builds bypasses authority invariants on
/// purpose, exactly like the attribution suite's raw writer).
fn raw_exec(database: &Path, sql: &str) -> Result<(), String> {
    let connection = Connection::open(database).expect("open raw writer");
    connection
        .busy_timeout(Duration::from_secs(5))
        .expect("busy timeout");
    connection
        .execute_batch(sql)
        .map_err(|error| error.to_string())
}

fn raw_query_u64(database: &Path, sql: &str) -> u64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get::<_, i64>(0))
        .expect("raw scalar query")
        .try_into()
        .expect("non-negative scalar")
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

/// The entry identity derivation, pinned here by replicating the crate's
/// domain-separated scheme: the simulated crash-window state must be
/// byte-indistinguishable from a real charge.
fn pin_credit_entry_id(payer_bytes: &[u8; 16], kind: i64, evidence: &[u8; 16]) -> [u8; 16] {
    let tag = b"nlos/topic/credit-entry/id/v1";
    let mut hasher = Sha256::new();
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag);
    for part in [
        payer_bytes.as_slice(),
        &kind.to_be_bytes(),
        evidence.as_slice(),
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    digest[..16].try_into().expect("digest has fixed length")
}

const ENTRY_KIND_SPEND: i64 = 3;

/// The opening ceremony: the durable account row with the exact grant, the
/// replay of the exact request, the drift and double-open rejections, and
/// the zero-payer rejection.
#[test]
fn open_credit_is_idempotent_and_double_open_fails_closed() {
    let harness = Harness::new("open");
    let request = OpenCreditRequest {
        payer: payer(7),
        initial_units: 10,
        idempotency_key: key(60),
        opened_at_ms: 3_000,
    };
    let opened = match harness.topics.open_credit(request).expect("open credit") {
        OpenCreditDecision::Opened(record) => record,
        OpenCreditDecision::Replayed(_) => panic!("fresh open cannot replay"),
    };
    assert_eq!(
        opened,
        CreditAccountRecord {
            payer: payer(7),
            balance_units: 10,
            total_granted_units: 10,
            total_spent_units: 0,
            opened_at_ms: 3_000,
            last_mutated_at_ms: 3_000,
        }
    );
    assert_eq!(balance(&harness.topics, payer(7)), opened);

    let replayed = match harness
        .topics
        .open_credit(request)
        .expect("replay open credit")
    {
        OpenCreditDecision::Replayed(record) => record,
        OpenCreditDecision::Opened(_) => panic!("identical open must replay"),
    };
    assert_eq!(replayed, opened);

    for drifted in [
        OpenCreditRequest {
            initial_units: 11,
            ..request
        },
        OpenCreditRequest {
            payer: payer(8),
            ..request
        },
    ] {
        assert!(matches!(
            harness.topics.open_credit(drifted),
            Err(TopicAuthorityError::IdempotencyConflict)
        ));
    }
    assert!(matches!(
        harness.topics.open_credit(OpenCreditRequest {
            payer: payer(7),
            initial_units: 10,
            idempotency_key: key(61),
            opened_at_ms: 3_001,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.topics.open_credit(OpenCreditRequest {
            payer: payer(0),
            initial_units: 10,
            idempotency_key: key(62),
            opened_at_ms: 3_002,
        }),
        Err(TopicAuthorityError::InvalidPolicy(_))
    ));
    assert_eq!(balance(&harness.topics, payer(7)), opened);
}

/// The charge on the publish admission chain: fitting payloads debit the
/// balance by the exact payload length; an insufficient balance rejects
/// with the typed error before any durable write (no publication row, no
/// spend entry, untouched balance) and the rejected key stays reusable.
#[test]
fn publish_charges_bytes_and_insufficiency_rejects_with_zero_state() {
    let harness = Harness::new("charge");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"metered",
        policy_for(payer(7)),
        101,
    );
    open_credit(&harness.topics, payer(7), 10, 60, 3_000);

    let first = publish_at(&harness.topics, topic.topic_id, 102, b"abcd", 8_001);
    assert_eq!(first.status, nlos_topic::PublicationStatus::Enqueued);
    assert_eq!(
        balance(&harness.topics, payer(7)),
        CreditAccountRecord {
            payer: payer(7),
            balance_units: 6,
            total_granted_units: 10,
            total_spent_units: 4,
            opened_at_ms: 3_000,
            last_mutated_at_ms: 8_001,
        }
    );

    match harness.topics.publish(PublishRequest {
        topic_id: topic.topic_id,
        payload: b"abcdefg".to_vec(),
        idempotency_key: key(103),
        published_at_ms: 8_002,
    }) {
        Err(TopicAuthorityError::InsufficientCredit {
            payer: rejected,
            balance_units,
            requested_units,
        }) => {
            assert_eq!(rejected, payer(7));
            assert_eq!(balance_units, 6);
            assert_eq!(requested_units, 7);
        }
        other => panic!("insufficient balance must reject typed, saw {other:?}"),
    }
    let database = harness.root.topic_db();
    assert_eq!(
        raw_query_u64(&database, "SELECT COUNT(*) FROM topic_publications"),
        1
    );
    assert_eq!(
        raw_query_u64(
            &database,
            "SELECT COUNT(*) FROM topic_credit_entries WHERE kind=3"
        ),
        1
    );
    assert_eq!(
        raw_query_u64(&database, "SELECT balance_units FROM topic_credit_accounts"),
        6
    );

    // The rejection consumed no idempotency key: after a recharge the same
    // key publishes normally.
    recharge(&harness.topics, payer(7), 5, 63, 8_010);
    let second = publish_at(&harness.topics, topic.topic_id, 103, b"abcdefg", 8_020);
    assert_eq!(second.status, nlos_topic::PublicationStatus::Enqueued);
    assert_eq!(balance(&harness.topics, payer(7)).balance_units, 4);

    // A funded-but-empty account gates immediately.
    let gated = create_topic(
        &harness.topics,
        head.channel_id,
        b"gated",
        policy_for(payer(8)),
        111,
    );
    open_credit(&harness.topics, payer(8), 0, 64, 3_100);
    match harness.topics.publish(PublishRequest {
        topic_id: gated.topic_id,
        payload: b"x".to_vec(),
        idempotency_key: key(104),
        published_at_ms: 8_030,
    }) {
        Err(TopicAuthorityError::InsufficientCredit {
            payer: rejected,
            balance_units,
            requested_units,
        }) => {
            assert_eq!(rejected, payer(8));
            assert_eq!(balance_units, 0);
            assert_eq!(requested_units, 1);
        }
        other => panic!("zero balance must reject typed, saw {other:?}"),
    }
    assert!(
        harness
            .topics
            .inspect_publications(gated.topic_id)
            .expect("no publications")
            .is_empty()
    );
}

/// Credit is opt-in per payer: a payer with no account publishes ungated,
/// and the readback fails closed for the unopened account.
#[test]
fn publish_without_account_is_ungated() {
    let harness = Harness::new("ungated");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"free",
        policy_for(payer(7)),
        101,
    );
    publish_at(&harness.topics, topic.topic_id, 102, b"abcd", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcdefg", 8_002);
    assert!(matches!(
        harness.topics.inspect_credit(payer(7)),
        Err(TopicAuthorityError::CreditAccountNotFound(id)) if id == payer(7)
    ));
    assert!(matches!(
        harness.topics.recharge_credit(RechargeCreditRequest {
            payer: payer(7),
            units: 5,
            idempotency_key: key(63),
            recharged_at_ms: 8_010,
        }),
        Err(TopicAuthorityError::CreditAccountNotFound(_))
    ));
}

/// The recharge appends units key-idempotently: replay adds nothing, drift
/// conflicts, an unknown payer and a zero-unit recharge fail closed, and
/// distinct keys sum.
#[test]
fn recharge_appends_idempotently_and_fails_closed() {
    let harness = Harness::new("recharge");
    open_credit(&harness.topics, payer(7), 5, 60, 3_000);
    let recharged = recharge(&harness.topics, payer(7), 7, 63, 8_000);
    assert_eq!(recharged.balance_units, 12);
    assert_eq!(recharged.total_granted_units, 12);
    assert_eq!(recharged.last_mutated_at_ms, 8_000);

    let replayed = match harness.topics.recharge_credit(RechargeCreditRequest {
        payer: payer(7),
        units: 7,
        idempotency_key: key(63),
        recharged_at_ms: 8_001,
    }) {
        Ok(RechargeCreditDecision::Replayed(record)) => record,
        other => panic!("identical recharge must replay, saw {other:?}"),
    };
    assert_eq!(replayed.balance_units, 12);
    let database = harness.root.topic_db();
    assert_eq!(
        raw_query_u64(
            &database,
            "SELECT COUNT(*) FROM topic_credit_entries WHERE kind=2"
        ),
        1
    );

    for drifted in [
        RechargeCreditRequest {
            units: 8,
            ..RechargeCreditRequest {
                payer: payer(7),
                units: 7,
                idempotency_key: key(63),
                recharged_at_ms: 8_000,
            }
        },
        RechargeCreditRequest {
            payer: payer(8),
            ..RechargeCreditRequest {
                payer: payer(7),
                units: 7,
                idempotency_key: key(63),
                recharged_at_ms: 8_000,
            }
        },
    ] {
        assert!(matches!(
            harness.topics.recharge_credit(drifted),
            Err(TopicAuthorityError::IdempotencyConflict)
        ));
    }
    let summed = recharge(&harness.topics, payer(7), 7, 65, 8_002);
    assert_eq!(summed.balance_units, 19);
    assert_eq!(summed.total_granted_units, 19);
    assert!(matches!(
        harness.topics.recharge_credit(RechargeCreditRequest {
            units: 0,
            ..RechargeCreditRequest {
                payer: payer(7),
                units: 7,
                idempotency_key: key(66),
                recharged_at_ms: 8_003,
            }
        }),
        Err(TopicAuthorityError::InvalidPolicy(_))
    ));
}

/// The crash-window simulation: the original attempt charged and durably
/// registered the publication, then died before the enqueue.  Replaying the
/// same key converges the enqueue without charging again — the spend entry
/// stays unique and the verified readback closes over the whole journal.
#[test]
fn resumed_pending_replay_does_not_double_charge() {
    let harness = Harness::new("resumed");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"crash",
        policy_for(payer(7)),
        101,
    );
    open_credit(&harness.topics, payer(7), 10, 60, 3_000);

    // A real publication of the same payload supplies the true payload
    // digest for the hand-built pending row.
    let free = create_topic(
        &harness.topics,
        head.channel_id,
        b"digest",
        policy_for(payer(9)),
        121,
    );
    let digest_source = publish_at(&harness.topics, free.topic_id, 0xE1, b"alpha", 8_500);
    let payload_digest = digest_source.payload_digest;

    // Hand-build the crash window: PENDING row plus the charge it committed.
    let database = harness.root.topic_db();
    let topic_hex = hex(topic.topic_id.as_bytes());
    let payer_hex = hex(payer(7).as_bytes());
    let key_hex = hex(key(0x60).as_bytes());
    let policy_hex = hex(&topic.policy_digest);
    let digest_hex = hex(&payload_digest);
    let evidence: [u8; 16] = *key(0x60).as_bytes();
    let entry_hex = hex(&pin_credit_entry_id(
        payer(7).as_bytes(),
        ENTRY_KIND_SPEND,
        &evidence,
    ));
    raw_exec(
        &database,
        &format!(
            "INSERT INTO topic_publications (
                 idempotency_key, topic_id, policy_digest, payer_account_id,
                 payload_digest, payload_bytes, status, channel_sequence,
                 channel_generation, cascade_budget_remaining, cascade_level,
                 parent_idempotency_key, published_at_ms, enqueued_at_ms
             ) VALUES (x'{key_hex}', x'{topic_hex}', x'{policy_hex}', x'{payer_hex}',
                       x'{digest_hex}', 5, 0, 0, 0, 2, 0, NULL, 9_000, 0);

             UPDATE topic_credit_accounts
                SET balance_units=balance_units-5,
                    total_spent_units=total_spent_units+5,
                    last_mutated_at_ms=9_000
              WHERE payer_account_id=x'{payer_hex}';

             INSERT INTO topic_credit_entries (
                 entry_id, payer_account_id, kind, units, idempotency_key,
                 evidence_key, recorded_at_ms
             ) VALUES (x'{entry_hex}', x'{payer_hex}', {ENTRY_KIND_SPEND}, 5,
                       NULL, x'{key_hex}', 9_000);",
        ),
    )
    .expect("simulate the charged pending row");
    assert_eq!(
        raw_query_u64(&database, "SELECT balance_units FROM topic_credit_accounts"),
        5
    );

    let resumed = match harness.topics.publish(PublishRequest {
        topic_id: topic.topic_id,
        payload: b"alpha".to_vec(),
        idempotency_key: key(0x60),
        published_at_ms: 9_100,
    }) {
        Ok(PublishDecision::Published(record)) => record,
        other => panic!("resumed publication must complete, saw {other:?}"),
    };
    assert_eq!(resumed.status, nlos_topic::PublicationStatus::Enqueued);
    assert_eq!(resumed.channel_sequence, digest_source.channel_sequence + 1);
    // The resumed path charged nothing: one spend entry, balance unchanged.
    assert_eq!(
        raw_query_u64(
            &database,
            &format!(
                "SELECT COUNT(*) FROM topic_credit_entries
                  WHERE kind={ENTRY_KIND_SPEND} AND evidence_key=x'{key_hex}'"
            )
        ),
        1
    );
    assert_eq!(
        raw_query_u64(&database, "SELECT balance_units FROM topic_credit_accounts"),
        5
    );
    assert_eq!(
        balance(&harness.topics, payer(7)),
        CreditAccountRecord {
            payer: payer(7),
            balance_units: 5,
            total_granted_units: 10,
            total_spent_units: 5,
            opened_at_ms: 3_000,
            last_mutated_at_ms: 9_000,
        }
    );
}

/// The republish charges the *child* topic's payer in the same pre-write
/// cluster: an insufficient child balance rejects before the parent's
/// cascade budget CAS (no budget spent), a recharge unblocks, and the
/// replay never charges twice.
#[test]
fn republish_charges_child_payer_and_insufficiency_spends_no_budget() {
    let harness = Harness::new("republish");
    let head = create_channel(&harness.channel, 1_024, 200);
    let parent_topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"parent",
        policy_for(payer(7)),
        101,
    );
    let child_topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"child",
        policy_for(payer(8)),
        102,
    );
    open_credit(&harness.topics, payer(8), 2, 64, 3_000);
    let parent = publish_at(&harness.topics, parent_topic.topic_id, 103, b"vwxyz", 8_001);

    match harness.topics.republish(nlos_topic::RepublishRequest {
        child_topic_id: child_topic.topic_id,
        parent_publication_key: parent.idempotency_key,
        payload: b"vwxyz".to_vec(),
        idempotency_key: key(104),
        republished_at_ms: 8_002,
    }) {
        Err(TopicAuthorityError::InsufficientCredit {
            payer: rejected,
            balance_units,
            requested_units,
        }) => {
            assert_eq!(rejected, payer(8));
            assert_eq!(balance_units, 2);
            assert_eq!(requested_units, 5);
        }
        other => panic!("insufficient child balance must reject typed, saw {other:?}"),
    }
    // The rejection ran before the budget CAS: the parent budget is intact.
    assert_eq!(
        harness
            .topics
            .inspect_publication(parent.idempotency_key)
            .expect("parent publication")
            .cascade_budget_remaining,
        2
    );

    recharge(&harness.topics, payer(8), 10, 65, 8_010);
    let child = match harness.topics.republish(nlos_topic::RepublishRequest {
        child_topic_id: child_topic.topic_id,
        parent_publication_key: parent.idempotency_key,
        payload: b"vwxyz".to_vec(),
        idempotency_key: key(104),
        republished_at_ms: 8_020,
    }) {
        Ok(nlos_topic::RepublishDecision::Republished(record)) => record,
        other => panic!("funded republish must succeed, saw {other:?}"),
    };
    assert_eq!(child.status, nlos_topic::PublicationStatus::Enqueued);
    let account = balance(&harness.topics, payer(8));
    assert_eq!(account.balance_units, 7);
    assert_eq!(account.total_spent_units, 5);
    assert_eq!(
        harness
            .topics
            .inspect_publication(parent.idempotency_key)
            .expect("parent publication")
            .cascade_budget_remaining,
        1
    );

    let replay = match harness.topics.republish(nlos_topic::RepublishRequest {
        child_topic_id: child_topic.topic_id,
        parent_publication_key: parent.idempotency_key,
        payload: b"vwxyz".to_vec(),
        idempotency_key: key(104),
        republished_at_ms: 8_030,
    }) {
        Ok(nlos_topic::RepublishDecision::Replayed(_)) => true,
        other => panic!("identical republish must replay, saw {other:?}"),
    };
    assert!(replay);
    assert_eq!(balance(&harness.topics, payer(8)), account);
}

/// The account survives a full close/reopen cycle byte-equal, stays gated,
/// and the additive v8 migration left the `user_version` watermark
/// untouched.
#[test]
fn account_survives_reopen_and_stays_gated() {
    let harness = Harness::new("reopen");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"durable",
        policy_for(payer(7)),
        101,
    );
    open_credit(&harness.topics, payer(7), 10, 60, 3_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abcd", 8_001);
    let before = balance(&harness.topics, payer(7));

    let version: i64 = {
        let connection = Connection::open(harness.root.topic_db()).expect("raw reader");
        connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version")
    };
    assert_eq!(
        version, 5,
        "the additive v8 step must not bump the watermark"
    );

    drop(harness.topics);
    let reopened = TopicAuthority::open(harness.root.0.clone(), Arc::clone(&harness.channel))
        .expect("reopen topic authority");
    assert_eq!(balance(&reopened, payer(7)), before);
    assert_eq!(balance(&reopened, payer(7)).balance_units, 6);
    // The gate survived the restart.
    match reopened.publish(PublishRequest {
        topic_id: topic.topic_id,
        payload: b"abcdefg".to_vec(),
        idempotency_key: key(103),
        published_at_ms: 8_002,
    }) {
        Err(TopicAuthorityError::InsufficientCredit { .. }) => {}
        other => panic!("insufficient balance must reject after reopen, saw {other:?}"),
    }
}

/// The credit journal is write-once and self-verifying: UPDATE and DELETE
/// are trigger-rejected, the account identity is frozen and its row CHECK
/// makes an inconsistent balance structurally impossible, and a tampered
/// journal (an orphan spend, a rewritten identity) fails closed at the
/// verified readback instead of returning an account.
// One test covers the whole tamper matrix: trigger-rejected mutation,
// frozen identity, CHECK-armed balance, orphan spend, and the rewritten
// identity — the fail-closed readback contract reads as one scenario.
#[allow(clippy::too_many_lines)]
#[test]
fn credit_journal_is_immutable_and_inspection_fails_closed() {
    let harness = Harness::new("immutable");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"frozen",
        policy_for(payer(7)),
        101,
    );
    open_credit(&harness.topics, payer(7), 10, 60, 3_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abcd", 8_001);
    let database = harness.root.topic_db();
    let payer_hex = hex(payer(7).as_bytes());

    let update_error = raw_exec(&database, "UPDATE topic_credit_entries SET units=99").unwrap_err();
    assert!(update_error.contains("immutable"), "{update_error}");
    let delete_error = raw_exec(&database, "DELETE FROM topic_credit_entries").unwrap_err();
    assert!(delete_error.contains("durable"), "{delete_error}");
    let account_delete_error =
        raw_exec(&database, "DELETE FROM topic_credit_accounts").unwrap_err();
    assert!(
        account_delete_error.contains("durable"),
        "{account_delete_error}"
    );
    let identity_error = raw_exec(
        &database,
        &format!(
            "UPDATE topic_credit_accounts
                SET open_idempotency_key=x'{}'
              WHERE payer_account_id=x'{payer_hex}'",
            hex(&[0x7F; 16])
        ),
    )
    .unwrap_err();
    assert!(
        identity_error.contains("identity is immutable"),
        "{identity_error}"
    );
    let check_error = raw_exec(
        &database,
        &format!(
            "UPDATE topic_credit_accounts
                SET balance_units=balance_units+1
              WHERE payer_account_id=x'{payer_hex}'"
        ),
    )
    .unwrap_err();
    assert!(
        check_error.contains("CHECK constraint failed"),
        "{check_error}"
    );
    assert_eq!(balance(&harness.topics, payer(7)).balance_units, 6);

    // An orphan spend — a charge whose publication never landed — is a torn
    // charge window and fails closed at the readback.
    let orphan = Harness::new("orphan");
    open_credit(&orphan.topics, payer(7), 10, 60, 3_000);
    let orphan_db = orphan.root.topic_db();
    let orphan_payer_hex = hex(payer(7).as_bytes());
    let orphan_evidence: [u8; 16] = *key(0xEE).as_bytes();
    let orphan_entry_hex = hex(&pin_credit_entry_id(
        payer(7).as_bytes(),
        ENTRY_KIND_SPEND,
        &orphan_evidence,
    ));
    raw_exec(
        &orphan_db,
        &format!(
            "INSERT INTO topic_credit_entries (
                 entry_id, payer_account_id, kind, units, idempotency_key,
                 evidence_key, recorded_at_ms
             ) VALUES (x'{orphan_entry_hex}', x'{orphan_payer_hex}',
                       {ENTRY_KIND_SPEND}, 5, NULL, x'{}', 9_000);",
            hex(key(0xEE).as_bytes())
        ),
    )
    .expect("insert the orphan spend");
    assert!(matches!(
        orphan.topics.inspect_credit(payer(7)),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));

    // A spend entry whose identity does not re-derive fails closed too.
    let rewritten = Harness::new("rewritten");
    let rewritten_head = create_channel(&rewritten.channel, 1_024, 200);
    let rewritten_topic = create_topic(
        &rewritten.topics,
        rewritten_head.channel_id,
        b"rewritten",
        policy_for(payer(7)),
        101,
    );
    open_credit(&rewritten.topics, payer(7), 10, 60, 3_000);
    publish_at(
        &rewritten.topics,
        rewritten_topic.topic_id,
        102,
        b"abcd",
        8_001,
    );
    let rewritten_db = rewritten.root.topic_db();
    let evidence_hex = hex(key(102).as_bytes());
    raw_exec(
        &rewritten_db,
        &format!(
            "UPDATE topic_credit_entries
                SET entry_id=x'{}'
              WHERE evidence_key=x'{evidence_hex}'",
            hex(&[0xFF; 16])
        )
        .replace(
            "UPDATE topic_credit_entries",
            "DROP TRIGGER topic_credit_entries_no_update; UPDATE topic_credit_entries",
        ),
    )
    .expect("rewrite the entry identity without its trigger");
    assert!(matches!(
        rewritten.topics.inspect_credit(payer(7)),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));
}

/// Strict separation of the two faces: with a charged account open, the
/// publish/advance cycle attributes exactly as the payer-metering suite
/// pins (byte-exact three-way identity), the attribution rows are
/// unchanged, and the credit totals move independently.
#[test]
fn attribution_identity_untouched_by_credit_face() {
    let harness = Harness::new("separation");
    let head = create_channel(&harness.channel, 1_024, 200);
    let topic = create_topic(
        &harness.topics,
        head.channel_id,
        b"metered",
        policy_for(payer(7)),
        101,
    );
    open_credit(&harness.topics, payer(7), 100, 60, 3_000);
    let a = subscribe_at(&harness.topics, topic.topic_id, 1, 8_000);
    publish_at(&harness.topics, topic.topic_id, 102, b"abc", 8_001);
    publish_at(&harness.topics, topic.topic_id, 103, b"abcd", 8_002);
    publish_at(&harness.topics, topic.topic_id, 104, b"vwxyz", 8_003);
    advance_subscription(&harness.topics, &a, 3, 8_100);

    assert_eq!(
        harness
            .topics
            .inspect_attribution(topic.topic_id)
            .expect("attribution report"),
        AttributionReport {
            topic_id: topic.topic_id,
            attributed_bytes: 12,
            unallocated_bytes: 0,
            unsettled_bytes: 0,
            total: 12,
            policy_version: nlos_topic::ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        }
    );
    let database = harness.root.topic_db();
    assert_eq!(
        raw_query_u64(&database, "SELECT COUNT(*) FROM topic_attribution_ledger"),
        3
    );
    // The credit face charged the same publications independently: 12 bytes
    // prepaid and spent, the attribution identity untouched.
    assert_eq!(
        balance(&harness.topics, payer(7)),
        CreditAccountRecord {
            payer: payer(7),
            balance_units: 88,
            total_granted_units: 100,
            total_spent_units: 12,
            opened_at_ms: 3_000,
            last_mutated_at_ms: 8_003,
        }
    );
}
