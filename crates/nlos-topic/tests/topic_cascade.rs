//! Cascade republish budget-spending tests (`RSM-FANOUT-001`).
//!
//! Every republish must consume exactly one unit of the parent publication's
//! cascade budget through the owner-side guarded CAS, stay inside the parent
//! policy's depth bound, remain idempotent across the budget-spend/enqueue
//! crash window, and keep the parent provenance chain auditable (monotone
//! levels to a root, no cycles, no broken links).

use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelAuthorityError, ChannelDecision, ChannelRecord,
    CreateChannelRequest, EnqueueDecision, EnqueueRequest,
};
use nlos_topic::{
    CreateTopicRequest, PublicationRecord, PublicationStatus, PublishDecision, PublishRequest,
    RepublishDecision, RepublishRequest, TopicAuthority, TopicAuthorityError, TopicDecision,
    TopicId, TopicPolicy, TopicRecord,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId};
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn payer() -> ResourceAccountId {
    ResourceAccountId::from_bytes([7; 16])
}

fn policy(cascade_depth: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients: 4,
        delivery_attempts: 3,
        cascade_depth,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(),
    }
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
            "nlos-topic-cascade-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
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
            Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
        let topics =
            TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
        Self {
            root,
            channel,
            topics,
        }
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
    cascade_depth: u64,
    seed: u8,
) -> TopicRecord {
    match topics
        .create_topic(CreateTopicRequest {
            channel_id,
            name: name.to_vec(),
            policy: policy(cascade_depth),
            idempotency_key: key(seed),
            created_at_ms: 2_000,
        })
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic cannot replay"),
    }
}

fn publish_at(
    topics: &TopicAuthority,
    topic_id: TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> PublicationRecord {
    match topics
        .publish(PublishRequest {
            topic_id,
            payload: payload.to_vec(),
            idempotency_key: key(seed),
            published_at_ms: at,
        })
        .expect("publish")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    }
}

fn republish_at(
    topics: &TopicAuthority,
    child_topic_id: TopicId,
    parent: IdempotencyKey,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> PublicationRecord {
    match topics
        .republish(RepublishRequest {
            child_topic_id,
            parent_publication_key: parent,
            payload: payload.to_vec(),
            idempotency_key: key(seed),
            republished_at_ms: at,
        })
        .expect("republish")
    {
        RepublishDecision::Republished(record) => record,
        RepublishDecision::Replayed(_) => panic!("fresh republish cannot replay"),
    }
}

fn direct_enqueue(
    channel: &ChannelAuthority,
    head: &ChannelRecord,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> nlos_channel::QueueEntryRecord {
    match channel
        .enqueue(EnqueueRequest {
            channel_id: head.channel_id,
            expected_generation: head.generation,
            expected_fencing_token: head.fencing_token,
            payload: payload.to_vec(),
            idempotency_key: key(seed),
            enqueued_at_ms: at,
        })
        .expect("direct enqueue")
    {
        EnqueueDecision::Enqueued(entry) => entry,
        EnqueueDecision::Replayed(_) => panic!("fresh direct enqueue cannot replay"),
    }
}

fn blob_param(value: &IdempotencyKey) -> &[u8] {
    value.as_bytes().as_slice()
}

#[test]
fn cascade_budget_spends_exactly_and_exhaustion_fails_closed() {
    let harness = Harness::new("budget");
    let head = create_channel(&harness.channel, 1_024, 200);
    let parent_topic = create_topic(&harness.topics, head.channel_id, b"budget.parent", 2, 31);
    let child_topic = create_topic(&harness.topics, head.channel_id, b"budget.child", 5, 32);

    let root = publish_at(&harness.topics, parent_topic.topic_id, 40, b"root", 5_000);
    assert_eq!(root.cascade_level, 0);
    assert_eq!(root.parent_publication_key, None);
    assert_eq!(root.cascade_budget_remaining, 2);

    // Two forwards spend the initial budget of exactly 2, one unit each.
    let first = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(40),
        51,
        b"fwd-one",
        5_100,
    );
    assert_eq!(first.cascade_level, 1);
    assert_eq!(first.parent_publication_key, Some(key(40)));
    // The child publication gets its own fresh budget from the child policy.
    assert_eq!(first.cascade_budget_remaining, 5);
    assert_eq!(first.status, PublicationStatus::Enqueued);
    assert_eq!(first.channel_sequence, 2);
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("audit root")
            .cascade_budget_remaining,
        1
    );

    let second = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(40),
        52,
        b"fwd-two",
        5_101,
    );
    assert_eq!(second.channel_sequence, 3);
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("audit spent root")
            .cascade_budget_remaining,
        0
    );

    // The N+1st forward fails closed with zero partial state: no child row,
    // no further enqueue and an unchanged (exhausted) budget.
    let error = harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"fwd-three".to_vec(),
            idempotency_key: key(53),
            republished_at_ms: 5_102,
        })
        .expect_err("budget exhausted");
    assert!(
        matches!(error, TopicAuthorityError::CascadeBudgetExhausted(parent) if parent == key(40)),
        "unexpected error: {error:?}"
    );
    assert!(matches!(
        harness.topics.inspect_publication(key(53)),
        Err(TopicAuthorityError::PublicationNotFound(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_publications(child_topic.topic_id)
            .expect("child journal")
            .len(),
        2
    );
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("budget unchanged after rejection")
            .cascade_budget_remaining,
        0
    );
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("queue")
            .max_sequence,
        3
    );
}

#[test]
fn cascade_depth_exceeded_fails_closed_pre_write() {
    let harness = Harness::new("depth");
    let head = create_channel(&harness.channel, 1_024, 200);
    let root_topic = create_topic(&harness.topics, head.channel_id, b"depth.root", 1, 31);
    let mid_topic = create_topic(&harness.topics, head.channel_id, b"depth.mid", 1, 32);
    let leaf_topic = create_topic(&harness.topics, head.channel_id, b"depth.leaf", 1, 33);

    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 5_000);

    // Level 1 stays within the parent (root) policy's cascade_depth of 1.
    let mid = republish_at(
        &harness.topics,
        mid_topic.topic_id,
        key(40),
        51,
        b"mid",
        5_100,
    );
    assert_eq!(mid.cascade_level, 1);

    // Level 2 exceeds the parent (mid publication) policy bound of 1.
    let error = harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: leaf_topic.topic_id,
            parent_publication_key: key(51),
            payload: b"leaf".to_vec(),
            idempotency_key: key(52),
            republished_at_ms: 5_101,
        })
        .expect_err("depth bound");
    assert!(
        matches!(
            error,
            TopicAuthorityError::CascadeDepthExceeded {
                parent_publication_key,
                requested_level: 2,
                cascade_depth: 1,
            } if parent_publication_key == key(51)
        ),
        "unexpected error: {error:?}"
    );

    // Zero partial state: no leaf publication, no row for the rejected key
    // and no budget movement on the mid publication.
    assert!(
        harness
            .topics
            .inspect_publications(leaf_topic.topic_id)
            .expect("leaf journal")
            .is_empty()
    );
    assert!(matches!(
        harness.topics.inspect_publication(key(52)),
        Err(TopicAuthorityError::PublicationNotFound(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(51))
            .expect("mid budget untouched")
            .cascade_budget_remaining,
        1
    );

    // The root's budget of 1 was spent by the level-1 forward: a second
    // forward passes the depth gate and hits the budget gate instead.
    let error = harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: leaf_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"leaf-2".to_vec(),
            idempotency_key: key(53),
            republished_at_ms: 5_102,
        })
        .expect_err("budget gate after depth gate");
    assert!(
        matches!(error, TopicAuthorityError::CascadeBudgetExhausted(parent) if parent == key(40)),
        "unexpected error: {error:?}"
    );
}

#[test]
fn republish_replay_spends_once_and_never_duplicates_the_enqueue() {
    let harness = Harness::new("replay");
    let head = create_channel(&harness.channel, 1_024, 200);
    let root_topic = create_topic(&harness.topics, head.channel_id, b"replay.root", 3, 31);
    let child_topic = create_topic(&harness.topics, head.channel_id, b"replay.child", 3, 32);
    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 6_000);

    let first = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(40),
        51,
        b"forward",
        6_100,
    );
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("spent exactly once")
            .cascade_budget_remaining,
        2
    );

    // The same key, parent and payload replay the original record.
    assert_eq!(
        harness
            .topics
            .republish(RepublishRequest {
                child_topic_id: child_topic.topic_id,
                parent_publication_key: key(40),
                payload: b"forward".to_vec(),
                idempotency_key: key(51),
                republished_at_ms: 9_999,
            })
            .expect("republish replay"),
        RepublishDecision::Replayed(first.clone())
    );
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("budget stable across replay")
            .cascade_budget_remaining,
        2
    );
    assert_eq!(
        harness
            .topics
            .inspect_publications(child_topic.topic_id)
            .expect("one child row")
            .len(),
        1
    );
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("no duplicate enqueue")
            .max_sequence,
        2
    );

    // Rebinding the same key to a different payload or parent conflicts.
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"other".to_vec(),
            idempotency_key: key(51),
            republished_at_ms: 6_101,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(41),
            payload: b"forward".to_vec(),
            idempotency_key: key(51),
            republished_at_ms: 6_102,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
}

#[test]
fn crash_window_budget_spent_pending_replay_supplements_once() {
    let harness = Harness::new("crash-window");
    let root_head = create_channel(&harness.channel, 1_024, 200);
    let child_head = create_channel(&harness.channel, 4, 201);
    let root_topic = create_topic(&harness.topics, root_head.channel_id, b"cw.root", 3, 31);
    let child_topic = create_topic(&harness.topics, child_head.channel_id, b"cw.child", 3, 32);
    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 7_000);

    // Fill the child channel to capacity: the republish's budget spend and
    // PENDING child row commit, but the enqueue itself is rejected.
    direct_enqueue(&harness.channel, &child_head, 61, b"aaaa", 7_001);
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"bb".to_vec(),
            idempotency_key: key(62),
            republished_at_ms: 7_002,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::QueueFull
        ))
    ));
    let pending = harness
        .topics
        .inspect_publication(key(62))
        .expect("audit pending child");
    assert_eq!(pending.status, PublicationStatus::PendingEnqueue);
    assert_eq!(pending.cascade_level, 1);
    assert_eq!(pending.parent_publication_key, Some(key(40)));
    assert_eq!(pending.cascade_budget_remaining, 3);
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("budget already spent")
            .cascade_budget_remaining,
        2
    );

    // The same-key replay after the capacity drains supplements (补投) the
    // enqueue without spending the budget again.
    harness
        .channel
        .ack(AckRequest {
            channel_id: child_head.channel_id,
            up_to_sequence: 1,
            acked_at_ms: 7_100,
        })
        .expect("drain capacity");
    let converged = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(40),
        62,
        b"bb",
        7_101,
    );
    assert_eq!(converged.status, PublicationStatus::Enqueued);
    assert_eq!(converged.channel_sequence, 2);
    assert_eq!(converged.cascade_level, 1);
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("spent exactly once overall")
            .cascade_budget_remaining,
        2
    );
    assert_eq!(
        harness
            .channel
            .inspect_queue(child_head.channel_id)
            .expect("child queue")
            .max_sequence,
        2
    );

    // A further replay returns the converged record with no new effects.
    assert_eq!(
        harness
            .topics
            .republish(RepublishRequest {
                child_topic_id: child_topic.topic_id,
                parent_publication_key: key(40),
                payload: b"bb".to_vec(),
                idempotency_key: key(62),
                republished_at_ms: 7_102,
            })
            .expect("replay converged republish"),
        RepublishDecision::Replayed(converged)
    );
    assert_eq!(
        harness
            .topics
            .inspect_publications(child_topic.topic_id)
            .expect("single child row")
            .len(),
        1
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the chain walk plus the tamper matrix.
fn parent_chain_walk_detects_cycles_and_broken_links() {
    let harness = Harness::new("chain");
    let head = create_channel(&harness.channel, 1_024, 200);
    let root_topic = create_topic(&harness.topics, head.channel_id, b"chain.root", 3, 31);
    let mid_topic = create_topic(&harness.topics, head.channel_id, b"chain.mid", 3, 32);
    let leaf_topic = create_topic(&harness.topics, head.channel_id, b"chain.leaf", 3, 33);

    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 8_000);
    republish_at(
        &harness.topics,
        mid_topic.topic_id,
        key(40),
        51,
        b"mid",
        8_001,
    );
    republish_at(
        &harness.topics,
        leaf_topic.topic_id,
        key(51),
        52,
        b"leaf",
        8_002,
    );

    // The auditable chain: leaf -> mid -> root with monotone levels, each
    // budget reconciling with its durable children.
    let leaf = harness
        .topics
        .inspect_publication(key(52))
        .expect("audit leaf");
    let mid = harness
        .topics
        .inspect_publication(leaf.parent_publication_key.expect("leaf parent"))
        .expect("audit mid");
    let root = harness
        .topics
        .inspect_publication(mid.parent_publication_key.expect("mid parent"))
        .expect("audit root");
    assert_eq!(
        (leaf.cascade_level, mid.cascade_level, root.cascade_level),
        (2, 1, 0)
    );
    assert_eq!(root.parent_publication_key, None);
    assert_eq!(root.cascade_budget_remaining, 2);
    assert_eq!(mid.cascade_budget_remaining, 2);
    assert_eq!(leaf.cascade_budget_remaining, 3);

    // Tamper the journal directly (the guard trigger must be dropped first —
    // itself evidence the frozen columns cannot be rewritten through the
    // owner — and the self-referential foreign key must be bypassed to forge
    // a dangling link): a self-cycle and a broken link must fail closed as
    // corrupt.
    let raw = Connection::open(harness.root.topic_db()).expect("raw topic db");
    raw.execute_batch(
        "PRAGMA foreign_keys=OFF;
         DROP TRIGGER topic_publications_commit_transition;",
    )
    .expect("drop guards for tampering");
    raw.execute(
        "UPDATE topic_publications SET parent_idempotency_key=?1 WHERE idempotency_key=?2",
        params![blob_param(&key(52)), blob_param(&key(52))],
    )
    .expect("tamper self-cycle");
    assert!(matches!(
        harness.topics.inspect_publication(key(52)),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));

    raw.execute(
        "UPDATE topic_publications SET parent_idempotency_key=?1 WHERE idempotency_key=?2",
        params![
            blob_param(&IdempotencyKey::from_bytes([0xEE; 16])),
            blob_param(&key(51))
        ],
    )
    .expect("tamper broken link");
    assert!(matches!(
        harness.topics.inspect_publication(key(51)),
        Err(TopicAuthorityError::CorruptRecord(_))
    ));

    // Restoring the chain and the guard makes the audit readable again.
    raw.execute(
        "UPDATE topic_publications SET parent_idempotency_key=?1 WHERE idempotency_key=?2",
        params![blob_param(&key(40)), blob_param(&key(51))],
    )
    .expect("restore mid parent");
    raw.execute(
        "UPDATE topic_publications SET parent_idempotency_key=?1 WHERE idempotency_key=?2",
        params![blob_param(&key(51)), blob_param(&key(52))],
    )
    .expect("restore leaf parent");
    raw.execute_batch(
        "CREATE TRIGGER topic_publications_commit_transition
         BEFORE UPDATE ON topic_publications
         WHEN NEW.idempotency_key != OLD.idempotency_key
             OR NEW.topic_id != OLD.topic_id
             OR NEW.policy_digest != OLD.policy_digest
             OR NEW.payer_account_id != OLD.payer_account_id
             OR NEW.payload_digest != OLD.payload_digest
             OR NEW.parent_idempotency_key IS NOT OLD.parent_idempotency_key
             OR NEW.cascade_level != OLD.cascade_level
             OR NEW.published_at_ms != OLD.published_at_ms
             OR NEW.cascade_budget_remaining > OLD.cascade_budget_remaining
             OR (NEW.cascade_budget_remaining != OLD.cascade_budget_remaining
                 AND OLD.status != 1)
             OR (OLD.status = 1
                 AND (NEW.channel_sequence != OLD.channel_sequence
                      OR NEW.channel_generation != OLD.channel_generation
                      OR NEW.enqueued_at_ms != OLD.enqueued_at_ms))
             OR (OLD.status != NEW.status
                 AND NOT (OLD.status = 0 AND NEW.status = 1))
         BEGIN
             SELECT RAISE(
                 ABORT,
                 'topic publication is immutable beyond enqueue commit and cascade budget spend'
             );
         END;",
    )
    .expect("recreate guard trigger");
    drop(raw);
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(52))
            .expect("restored chain audits"),
        leaf
    );
}

#[test]
fn multi_level_chain_budgets_levels_and_restart_durability() {
    let root = Root::new("levels");
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("open topic authority");
    let head = create_channel(&channel, 1_024, 200);
    let root_topic = create_topic(&topics, head.channel_id, b"levels.root", 3, 31);
    let child_topic = create_topic(&topics, head.channel_id, b"levels.child", 3, 32);
    let grand_topic = create_topic(&topics, head.channel_id, b"levels.grand", 3, 33);

    let root_pub = publish_at(&topics, root_topic.topic_id, 40, b"root", 9_000);
    let child_pub = republish_at(&topics, child_topic.topic_id, key(40), 51, b"child", 9_001);
    let grand_pub = republish_at(&topics, grand_topic.topic_id, key(51), 52, b"grand", 9_002);

    assert_eq!(root_pub.cascade_level, 0);
    assert_eq!(child_pub.cascade_level, 1);
    assert_eq!(grand_pub.cascade_level, 2);
    assert_eq!(child_pub.parent_publication_key, Some(key(40)));
    assert_eq!(grand_pub.parent_publication_key, Some(key(51)));

    // Each level's budget reconciles with its own policy minus its children.
    assert_eq!(
        topics
            .inspect_publication(key(40))
            .expect("root audit")
            .cascade_budget_remaining,
        2
    );
    assert_eq!(
        topics
            .inspect_publication(key(51))
            .expect("child audit")
            .cascade_budget_remaining,
        2
    );
    assert_eq!(
        topics
            .inspect_publication(key(52))
            .expect("grand audit")
            .cascade_budget_remaining,
        3
    );

    // The root publish itself still replays without re-initializing or
    // touching the partially spent budget.
    assert!(matches!(
        topics.publish(PublishRequest {
            topic_id: root_topic.topic_id,
            payload: b"root".to_vec(),
            idempotency_key: key(40),
            published_at_ms: 9_999,
        }),
        Ok(PublishDecision::Replayed(_))
    ));
    assert_eq!(
        topics
            .inspect_publication(key(40))
            .expect("budget after publish replay")
            .cascade_budget_remaining,
        2
    );

    // Capture the post-spend audit state (the root and child budgets were
    // mutated by their children after the publish-time snapshots above).
    let audits_before_restart = [
        topics.inspect_publication(key(40)).expect("root audit"),
        topics.inspect_publication(key(51)).expect("child audit"),
        topics.inspect_publication(key(52)).expect("grand audit"),
    ];

    // Restart: every cascade binding replays field-for-field.
    drop(topics);
    drop(channel);
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("reopen channel authority"));
    let topics =
        TopicAuthority::open(root.path(), Arc::clone(&channel)).expect("reopen topic authority");
    assert_eq!(
        topics
            .inspect_publication(key(40))
            .expect("root after restart"),
        audits_before_restart[0]
    );
    assert_eq!(
        topics
            .inspect_publication(key(51))
            .expect("child after restart"),
        audits_before_restart[1]
    );
    assert_eq!(
        topics
            .inspect_publication(key(52))
            .expect("grand after restart"),
        audits_before_restart[2]
    );
}

#[test]
fn exhausted_parent_still_replays_earlier_successful_keys() {
    let harness = Harness::new("exhausted-replay");
    let head = create_channel(&harness.channel, 1_024, 200);
    let root_topic = create_topic(&harness.topics, head.channel_id, b"x.root", 1, 31);
    let child_topic = create_topic(&harness.topics, head.channel_id, b"x.child", 3, 32);
    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 6_000);

    let spent = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(40),
        51,
        b"only",
        6_100,
    );
    let error = harness
        .topics
        .republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"second".to_vec(),
            idempotency_key: key(52),
            republished_at_ms: 6_101,
        })
        .expect_err("exhausted");
    assert!(
        matches!(error, TopicAuthorityError::CascadeBudgetExhausted(parent) if parent == key(40)),
        "unexpected error: {error:?}"
    );

    // The earlier success still replays its original decision after the
    // budget reached zero, with no budget movement and no new enqueue.
    assert_eq!(
        harness
            .topics
            .republish(RepublishRequest {
                child_topic_id: child_topic.topic_id,
                parent_publication_key: key(40),
                payload: b"only".to_vec(),
                idempotency_key: key(51),
                republished_at_ms: 9_999,
            })
            .expect("replay after exhaustion"),
        RepublishDecision::Replayed(spent.clone())
    );
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(40))
            .expect("budget stays at zero")
            .cascade_budget_remaining,
        0
    );
    assert_eq!(
        harness
            .channel
            .inspect_queue(head.channel_id)
            .expect("no enqueue growth")
            .max_sequence,
        2
    );

    // The rejected key left no durable row and keeps failing.
    assert!(matches!(
        harness.topics.inspect_publication(key(52)),
        Err(TopicAuthorityError::PublicationNotFound(_))
    ));
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: b"second".to_vec(),
            idempotency_key: key(52),
            republished_at_ms: 6_102,
        }),
        Err(TopicAuthorityError::CascadeBudgetExhausted(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the full parent-state and binding-drift matrix.
fn republish_validates_parent_state_and_request_bindings() {
    let harness = Harness::new("validate");
    let root_head = create_channel(&harness.channel, 1_024, 200);
    let tiny_head = create_channel(&harness.channel, 4, 201);
    let root_topic = create_topic(&harness.topics, root_head.channel_id, b"v.root", 3, 31);
    let child_topic = create_topic(&harness.topics, root_head.channel_id, b"v.child", 3, 32);
    let other_topic = create_topic(&harness.topics, root_head.channel_id, b"v.other", 3, 34);
    publish_at(&harness.topics, root_topic.topic_id, 40, b"root", 6_000);

    // A missing parent fails closed.
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: IdempotencyKey::from_bytes([0xEE; 16]),
            payload: b"forward".to_vec(),
            idempotency_key: key(50),
            republished_at_ms: 6_001,
        }),
        Err(TopicAuthorityError::PublicationNotFound(_))
    ));

    // An unknown child topic fails closed.
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: TopicId::from_bytes([0x00; 16]),
            parent_publication_key: key(40),
            payload: b"forward".to_vec(),
            idempotency_key: key(50),
            republished_at_ms: 6_002,
        }),
        Err(TopicAuthorityError::TopicNotFound(_))
    ));

    // An empty payload fails closed.
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(40),
            payload: Vec::new(),
            idempotency_key: key(50),
            republished_at_ms: 6_003,
        }),
        Err(TopicAuthorityError::InvalidPayload)
    ));

    // A parent still inside its PENDING_ENQUEUE window is not forwardable,
    // and the rejection spends none of its budget.
    let pending_topic = create_topic(&harness.topics, tiny_head.channel_id, b"v.pending", 2, 33);
    direct_enqueue(&harness.channel, &tiny_head, 61, b"aaaa", 6_004);
    assert!(matches!(
        harness.topics.publish(PublishRequest {
            topic_id: pending_topic.topic_id,
            payload: b"pp".to_vec(),
            idempotency_key: key(62),
            published_at_ms: 6_005,
        }),
        Err(TopicAuthorityError::Channel(
            ChannelAuthorityError::QueueFull
        ))
    ));
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: child_topic.topic_id,
            parent_publication_key: key(62),
            payload: b"forward".to_vec(),
            idempotency_key: key(51),
            republished_at_ms: 6_006,
        }),
        Err(TopicAuthorityError::PublicationNotEnqueued(_))
    ));
    assert_eq!(
        harness
            .topics
            .inspect_publication(key(62))
            .expect("pending parent audit")
            .cascade_budget_remaining,
        2
    );
    assert!(
        harness
            .topics
            .inspect_publications(child_topic.topic_id)
            .expect("no child rows")
            .is_empty()
    );

    // Once the parent reaches its terminal ENQUEUED state the same forward
    // is admitted.
    harness
        .channel
        .ack(AckRequest {
            channel_id: tiny_head.channel_id,
            up_to_sequence: 1,
            acked_at_ms: 6_100,
        })
        .expect("drain capacity");
    publish_at(&harness.topics, pending_topic.topic_id, 62, b"pp", 6_101);
    let forwarded = republish_at(
        &harness.topics,
        child_topic.topic_id,
        key(62),
        51,
        b"forward",
        6_102,
    );
    assert_eq!(forwarded.cascade_level, 1);

    // Binding drift: the same key forwarded to a different child topic
    // conflicts.
    assert!(matches!(
        harness.topics.republish(RepublishRequest {
            child_topic_id: other_topic.topic_id,
            parent_publication_key: key(62),
            payload: b"forward".to_vec(),
            idempotency_key: key(51),
            republished_at_ms: 6_103,
        }),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
}
