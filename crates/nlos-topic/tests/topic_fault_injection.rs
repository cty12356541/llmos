//! B-TOPIC-001 (lane Y): kill-window / fault-injection matrix for the
//! durable Topic service-layer authority — `TopicAuthority::create_topic`,
//! `subscribe`, `publish` (the verify-then-commit `PENDING_ENQUEUE` window),
//! `advance` (per-subscriber cursor CAS) and `compact` (min-live-cursor
//! clamp delegated to `ChannelAuthority::compact`).
//!
//! Harness and fixtures follow the established matrices exactly, most
//! recently `nlos-channel/tests/channel_fault_injection.rs` (kill-9 children
//! synchronized through piped `READY` markers — never sleeps, `FAULT_LOCK`
//! process-wide serialization, WAL tail truncation, typed error-chain
//! assertions, raw table counts, `PRAGMA integrity_check` per scenario) and
//! before that `nlos-task/tests/fault_injection.rs`,
//! `nlos-resource/tests/finalize_fault_injection.rs` (W1–W6 matrix row
//! shape, Phase A/B power-loss structure).
//!
//! **Fault-VFS plumbing (documented harness constraint, same deviation as
//! the channel matrix)**: `TopicAuthority` has no `open_with_vfs`
//! constructor and the workspace forbids `unsafe`, so the shim is routed in
//! through a `SQLite` **URI filename**: `rusqlite`'s `Connection::open` sets
//! `SQLITE_OPEN_URI`, and both `TopicAuthority::open` and
//! `ChannelAuthority::open` pass `root.join("…-authority.db")` through
//! unchanged, so a root of `file:<db>?vfs=<shim>&tail=` routes that one
//! authority connection through the registered fault VFS (the appended
//! `/<name>-authority.db` tail lands in the ignored `tail=` query
//! parameter). The junk directory that the authority `create_dir_all(root)`
//! call creates for the literal URI path is kept inside a RAII sandbox
//! process CWD — the worktree is never touched. Every reopen / raw reader /
//! integrity check uses the plain default VFS and can never be faulted.
//!
//! **Fault targeting**: each scenario routes exactly ONE authority
//! connection (the topic authority for W1/W2/W3/W5/CAS windows; the channel
//! authority for the W6 compact window, whose only durable write is the
//! channel trim) through the shim; the other authority always stays on the
//! plain VFS, so a "power loss" on one authority's file never disturbs the
//! other's durable bytes — that asymmetry is precisely what makes the
//! cross-authority `PENDING_ENQUEUE` windows observable.
//!
//! Matrix (window × scenario):
//! - W1 pre-commit IOERR (`create_topic` + subscribe + advance + publish) —
//!   typed `Sqlite` error whose chain names the injected condition, zero
//!   phantom rows after reopen, the unfaulted same request converges; the
//!   publish variant additionally proves the channel side was never touched
//!   (zero queue entries);
//! - W2 pre-commit ENOSPC (`create_topic` + publish) — same convergence with
//!   `SQLITE_FULL`;
//! - W3 commit-point `PowerLossAfter` both directions (`create_topic` +
//!   publish): invisible (Phase A, page-cache loss modeled) — the publish
//!   case loses the whole topic side while the channel enqueue durably
//!   landed, and replaying the same key converges onto `ENQUEUED` with the
//!   channel holding exactly one entry; visible (Phase B, kill-9 after
//!   commit) — replay is byte-equal `Replayed` and never duplicates the
//!   enqueue;
//! - W4 torn WAL tail — topic side on the create and publish paths: every
//!   representative cut inside the final transaction frame span (and one
//!   commit deeper on the publish path) leaves the publication either
//!   wholly invisible (redo-able) or `PENDING_ENQUEUE` (the declared legal
//!   crash window — its convergence path is asserted, not its absence);
//!   both branches converge to the identical `ENQUEUED` record with the
//!   channel still holding exactly one entry per key. Channel side on the
//!   publish path: the enqueue transaction vanishes whole (entry +
//!   bookkeeping), the topic journal keeps its `ENQUEUED` binding dangling,
//!   every cross-check fails closed `CorruptRecord` and the same-key replay
//!   stays idempotent — never a soft success over the lost entry;
//! - W5 replay storm (publish + subscribe + advance) — same request
//!   replayed 3+ times plus once after reopen: every call returns the
//!   byte-equal original record, exactly one row set, no duplicate enqueue;
//! - W6 advance CAS kill-window — crash before the cursor CAS is durable
//!   leaves the cursor at the old value (redo-able), after it at the new
//!   value (replay-able); regression / over-range advances always fail
//!   `InvalidSequence`; a sibling subscriber's cursor never moves;
//! - W7 compact kill-window — a power loss on the channel trim leaves the
//!   min-live-cursor bound and both subscriber cursors intact and the redo
//!   converges; a kill-9 after the trim is durable replays byte-equal; the
//!   trim never crosses the min live subscriber cursor.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death,
//! so a killed process is NOT a machine power loss. Writes the kernel
//! accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//! The channel-side torn-tail row of W4 additionally models an fsync-region
//! tail loss (a channel commit that was already durable when the topic's
//! later `ENQUEUED` commit ran) — a disk-level torn-write corruption that
//! lies outside any process-crash window and is documented per-assertion
//! below.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established repo
//! shape (all prior `*_fault_injection.rs` files are monolithic); fixtures
//! are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::{
    AckRequest, ChannelAuthority, ChannelDecision, ChannelRecord, CompactReceipt,
    CreateChannelRequest,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_topic::{
    AdvanceDecision, AdvanceRequest, CreateTopicRequest, PublicationRecord, PublicationStatus,
    PublishDecision, PublishRequest, SubscribeDecision, SubscribeRequest, SubscriberKey,
    SubscriptionRecord, TopicAuthority, TopicAuthorityError, TopicCompactDecision,
    TopicCompactReceipt, TopicDecision, TopicPolicy, TopicRecord,
};
use nlos_types::{ChannelId, Generation, IdempotencyKey, ResourceAccountId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-topic-fault";

const CAPACITY_BYTES: u64 = 1_024;
const CHANNEL_CREATED_AT_MS: u64 = 1_000;
const TOPIC_CREATED_AT_MS: u64 = 2_000;
const SUBSCRIBED_AT_MS: u64 = 3_000;
const SUBSCRIBER_B_AT_MS: u64 = 3_200;
const PUBLISH_ONE_AT_MS: u64 = 4_000;
const PUBLISH_TWO_AT_MS: u64 = 4_001;
const PUBLISH_THREE_AT_MS: u64 = 4_002;
const ADVANCED_AT_MS: u64 = 5_000;
const ACKED_AT_MS: u64 = 5_100;

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn payer() -> ResourceAccountId {
    ResourceAccountId::from_bytes([0x07; 16])
}

fn subscriber(seed: u8) -> SubscriberKey {
    SubscriberKey::from_bytes([seed; 16])
}

fn policy_for(max_recipients: u64) -> TopicPolicy {
    TopicPolicy {
        max_recipients,
        delivery_attempts: 3,
        cascade_depth: 2,
        retained_bytes: 4_096,
        retention_ms: 86_400_000,
        payer: payer(),
    }
}

// ---------------------------------------------------------------------------
// request builders and decision unwrap helpers
// ---------------------------------------------------------------------------

fn create_channel_request() -> nlos_channel::CreateChannelRequest {
    CreateChannelRequest {
        capacity_bytes: CAPACITY_BYTES,
        policy_digest: [0x44; 32],
        idempotency_key: key(0xB1),
        created_at_ms: CHANNEL_CREATED_AT_MS,
    }
}

fn create_topic_request(channel_id: ChannelId) -> nlos_topic::CreateTopicRequest {
    CreateTopicRequest {
        channel_id,
        name: b"fault.matrix".to_vec(),
        policy: policy_for(8),
        idempotency_key: key(0xB2),
        created_at_ms: TOPIC_CREATED_AT_MS,
    }
}

fn subscribe_request(
    topic_id: nlos_topic::TopicId,
    seed: u8,
    at: u64,
) -> nlos_topic::SubscribeRequest {
    SubscribeRequest {
        topic_id,
        subscriber_key: subscriber(seed),
        subscribed_at_ms: at,
    }
}

fn publish_request(
    topic_id: nlos_topic::TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> nlos_topic::PublishRequest {
    PublishRequest {
        topic_id,
        payload: payload.to_vec(),
        idempotency_key: key(seed),
        published_at_ms: at,
    }
}

fn advance_request(
    topic_id: nlos_topic::TopicId,
    seed: u8,
    up_to_sequence: u64,
    at: u64,
) -> nlos_topic::AdvanceRequest {
    AdvanceRequest {
        topic_id,
        subscriber_key: subscriber(seed),
        up_to_sequence,
        advanced_at_ms: at,
    }
}

fn created_channel(
    decision: Result<ChannelDecision, nlos_channel::ChannelAuthorityError>,
) -> ChannelRecord {
    match decision.expect("create channel") {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh channel create cannot replay"),
    }
}

fn created_topic(authority: &TopicAuthority, channel_id: ChannelId) -> TopicRecord {
    match authority
        .create_topic(create_topic_request(channel_id))
        .expect("create topic")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh topic create cannot replay"),
    }
}

fn subscribed(
    authority: &TopicAuthority,
    topic_id: nlos_topic::TopicId,
    seed: u8,
    at: u64,
) -> SubscriptionRecord {
    match authority
        .subscribe(subscribe_request(topic_id, seed, at))
        .expect("subscribe")
    {
        SubscribeDecision::Subscribed(record) => record,
        SubscribeDecision::Replayed(_) => panic!("fresh subscribe cannot replay"),
    }
}

fn published(
    authority: &TopicAuthority,
    topic_id: nlos_topic::TopicId,
    seed: u8,
    payload: &[u8],
    at: u64,
) -> PublicationRecord {
    match authority
        .publish(publish_request(topic_id, seed, payload, at))
        .expect("publish")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    }
}

fn replayed_publish(
    authority: &TopicAuthority,
    request: nlos_topic::PublishRequest,
) -> PublicationRecord {
    match authority.publish(request).expect("publish replay") {
        PublishDecision::Replayed(record) => record,
        PublishDecision::Published(_) => panic!("expected Replayed, got Published"),
    }
}

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open (channel-matrix deviation note)
// ---------------------------------------------------------------------------

/// RAII test root: one fresh directory per scenario, removed on drop.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-topic-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// RAII sandbox process CWD. The fault-VFS open runs `create_dir_all(root)`
/// on the literal URI root string, which is a relative OS path; the sandbox
/// keeps that junk directory tree inside a temp directory that is removed on
/// drop, so the worktree stays clean. All fault tests are serialized by
/// `FAULT_LOCK`, and every other test in this binary is either a no-op
/// (`crash_child_helper` without the scenario environment) or uses absolute
/// paths only.
struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-topic-fault-cwd-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create sandbox cwd");
        let previous = std::env::current_dir().expect("capture previous cwd");
        std::env::set_current_dir(&directory).expect("enter sandbox cwd");
        Self {
            previous,
            directory,
        }
    }
}

impl Drop for SandboxCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn topic_database(base: &Path) -> PathBuf {
    base.join("topic-authority.db")
}

fn channel_database(base: &Path) -> PathBuf {
    base.join("channel-authority.db")
}

/// The URI root that routes the topic authority's connection through the
/// registered fault VFS (see the header deviation note).
fn fault_topic_root(base: &Path) -> String {
    format!(
        "file:{}?vfs={VFS_NAME}&tail=",
        topic_database(base).display()
    )
}

/// The URI root that routes the channel authority's connection through the
/// registered fault VFS (used only by the compact kill-window, whose only
/// durable write is the channel trim).
fn fault_channel_root(base: &Path) -> String {
    format!(
        "file:{}?vfs={VFS_NAME}&tail=",
        channel_database(base).display()
    )
}

fn register_fault_vfs() {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
}

/// Opens the topic authority with its connection on the fault VFS while the
/// channel authority stays on the plain VFS. Pragmas and the migration run
/// while faults are disarmed, so the schema prefix is always durable before
/// any injection.
fn open_topic_fault(base: &Path, channel: &Arc<ChannelAuthority>) -> TopicAuthority {
    register_fault_vfs();
    TopicAuthority::open(fault_topic_root(base), Arc::clone(channel))
        .expect("open topic authority via fault vfs")
}

/// Opens the channel authority with its connection on the fault VFS; the
/// topic authority is opened on the plain root afterwards.
fn open_channel_fault(base: &Path) -> Arc<ChannelAuthority> {
    register_fault_vfs();
    Arc::new(
        ChannelAuthority::open(fault_channel_root(base))
            .expect("open channel authority via fault vfs"),
    )
}

fn reopen_channel(base: &Path) -> Arc<ChannelAuthority> {
    Arc::new(ChannelAuthority::open(base).expect("reopen channel authority"))
}

fn reopen_topics(base: &Path, channel: &Arc<ChannelAuthority>) -> TopicAuthority {
    TopicAuthority::open(base, Arc::clone(channel)).expect("reopen topic authority")
}

// ---------------------------------------------------------------------------
// shared assertions (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of a `TopicAuthorityError`, top cause last, for
/// content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
fn error_chain(error: &TopicAuthorityError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Asserts a typed storage failure whose cause chain names the injected
/// condition (`"i/o"` / `"ioerr"` / `"full"`): never a fake success, never
/// a panic.
fn assert_sqlite_error_chain(error: &TopicAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, TopicAuthorityError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
    );
}

fn raw_count(database: &Path, sql: &str) -> i64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

/// Row counts of the three topic tables: `topics`, `topic_subscriptions`,
/// `topic_publications`.
fn assert_topic_counts(base: &Path, expected: [i64; 3]) {
    let tables = ["topics", "topic_subscriptions", "topic_publications"];
    for (table, want) in tables.iter().zip(expected) {
        assert_eq!(
            raw_count(
                &topic_database(base),
                &format!("SELECT COUNT(*) FROM {table}")
            ),
            want,
            "unexpected row count in {table}"
        );
    }
}

/// The durable cursor of one subscription row, read out-of-band.
fn raw_subscriber_cursor(base: &Path, topic_id: &nlos_topic::TopicId, seed: u8) -> i64 {
    let connection = Connection::open(topic_database(base)).expect("open raw reader");
    connection
        .query_row(
            "SELECT cursor FROM topic_subscriptions
             WHERE topic_id=?1 AND subscriber_key=?2",
            rusqlite::params![
                topic_id.as_bytes().as_slice(),
                subscriber(seed).as_bytes().as_slice()
            ],
            |row| row.get(0),
        )
        .expect("read subscriber cursor")
}

fn raw_channel_entries(base: &Path) -> i64 {
    raw_count(
        &channel_database(base),
        "SELECT COUNT(*) FROM channel_queue_entries",
    )
}

fn raw_entries_for_key(base: &Path, seed: u8) -> i64 {
    let connection = Connection::open(channel_database(base)).expect("open raw reader");
    connection
        .query_row(
            "SELECT COUNT(*) FROM channel_queue_entries WHERE idempotency_key=?1",
            [key(seed).as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("count entries for key")
}

fn raw_channel_max_sequence(base: &Path) -> i64 {
    let connection = Connection::open(channel_database(base)).expect("open raw reader");
    connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM channel_queue_entries",
            [],
            |row| row.get(0),
        )
        .expect("read max sequence")
}

fn raw_channel_water(base: &Path, column: &str) -> i64 {
    let connection = Connection::open(channel_database(base)).expect("open raw reader");
    connection
        .query_row(
            &format!("SELECT {column} FROM channel_queue_cursors"),
            [],
            |row| row.get(0),
        )
        .expect("read queue watermark")
}

fn assert_integrity(database: &Path) {
    let connection = Connection::open(database).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn assert_integrity_both(base: &Path) {
    assert_integrity(&topic_database(base));
    assert_integrity(&channel_database(base));
}

/// Asserts the exact `ENQUEUED` shape of one publication record against the
/// fixture constants (the payload digest is bound, not the payload body).
fn assert_enqueued(record: &PublicationRecord, sequence: u64, published_at_ms: u64) {
    assert_eq!(record.status, PublicationStatus::Enqueued);
    assert_eq!(record.channel_sequence, sequence);
    assert_eq!(record.channel_generation, Generation::INITIAL.get());
    assert_eq!(record.published_at_ms, published_at_ms);
    assert_eq!(record.enqueued_at_ms, published_at_ms);
    assert_eq!(record.cascade_budget_remaining, policy_for(8).cascade_depth);
    assert_eq!(record.payer, payer());
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_TOPIC_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_TOPIC_CRASH_CHILD_ROOT", root.base().as_os_str())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY` marker (pipe synchronization,
/// no sleeps); kills and reaps the child on timeout or early exit.
fn await_marker(child: &mut Child) -> String {
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
        Ok(Ok(marker)) => marker,
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

fn hex_encode(value: &[u8]) -> String {
    use std::fmt::Write as _;
    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn hex_decode16(text: &str) -> [u8; 16] {
    assert_eq!(text.len(), 32, "id hex is 16 bytes");
    let mut decoded = [0_u8; 16];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    decoded
}

fn hex_decode32(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "fence hex is 32 bytes");
    let mut decoded = [0_u8; 32];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    decoded
}

fn marker_part(marker: &str, index: usize) -> &str {
    marker
        .trim()
        .strip_prefix("READY ")
        .expect("marker")
        .split(' ')
        .nth(index)
        .unwrap_or_else(|| panic!("marker part {index}"))
}

/// Decodes the plain id markers: `READY <topic-id> <channel-id>`.
struct IdMarker {
    topic_id: nlos_topic::TopicId,
    channel_id: ChannelId,
}

fn decode_id_marker(marker: &str) -> IdMarker {
    IdMarker {
        topic_id: nlos_topic::TopicId::from_bytes(hex_decode16(marker_part(marker, 0))),
        channel_id: ChannelId::from_bytes(hex_decode16(marker_part(marker, 1))),
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_TOPIC_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_TOPIC_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "topic-create-commit" => child_topic_create_commit(&root),
        "publish-one-commit" => child_publish_one_commit(&root),
        "publish-two-commit" => child_publish_two_commit(&root),
        "compact-commit" => child_compact_commit(&root),
        other => panic!("unknown crash child scenario {other}"),
    }
}

fn child_pair(root: &Path) -> (Arc<ChannelAuthority>, TopicAuthority) {
    let channel = reopen_channel(root);
    let topics = reopen_topics(root, &channel);
    (channel, topics)
}

/// Child fixture: one fully committed `create_topic` on the plain VFS; the
/// kill lands AFTER the commit point. Marker:
/// `READY <topic-id> <channel-id> <fence>`.
fn child_topic_create_commit(root: &Path) -> ! {
    let (channel, topics) = child_pair(root);
    let head = created_channel(channel.create_channel(create_channel_request()));
    let topic = created_topic(&topics, head.channel_id);
    announce(&format!(
        "READY {} {} {}",
        hex_encode(topic.topic_id.as_bytes()),
        hex_encode(topic.channel_id.as_bytes()),
        hex_encode(topic.channel_fencing_token.as_slice()),
    ));
    let _keeper = (channel, topics);
    loop {
        std::thread::park();
    }
}

/// Child fixture: channel + topic + one fully committed publish. Marker:
/// `READY <topic-id> <channel-id>`.
fn child_publish_one_commit(root: &Path) -> ! {
    let (channel, topics) = child_pair(root);
    let head = created_channel(channel.create_channel(create_channel_request()));
    let topic = created_topic(&topics, head.channel_id);
    published(&topics, topic.topic_id, 0xD1, b"alpha", PUBLISH_ONE_AT_MS);
    announce(&format!(
        "READY {} {}",
        hex_encode(topic.topic_id.as_bytes()),
        hex_encode(topic.channel_id.as_bytes()),
    ));
    let _keeper = (channel, topics);
    loop {
        std::thread::park();
    }
}

/// Child fixture: channel + topic + one subscriber + two fully committed
/// publishes. The topic WAL's final transactions are `pending2` then
/// `enqueued2`; the channel WAL's final transaction is the second enqueue.
/// Marker: `READY <topic-id> <channel-id>`.
fn child_publish_two_commit(root: &Path) -> ! {
    let (channel, topics) = child_pair(root);
    let head = created_channel(channel.create_channel(create_channel_request()));
    let topic = created_topic(&topics, head.channel_id);
    subscribed(&topics, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    published(&topics, topic.topic_id, 0xD1, b"alpha", PUBLISH_ONE_AT_MS);
    published(&topics, topic.topic_id, 0xD2, b"beta", PUBLISH_TWO_AT_MS);
    announce(&format!(
        "READY {} {}",
        hex_encode(topic.topic_id.as_bytes()),
        hex_encode(topic.channel_id.as_bytes()),
    ));
    let _keeper = (channel, topics);
    loop {
        std::thread::park();
    }
}

/// Child fixture: the compact scenario prefix (two subscribers at cursors 0
/// and 3, three publications, the owner ack to 3, A advanced to 2) plus one
/// fully committed `compact(topic, 9)` trimmed to the min live cursor 2.
/// Marker: `READY <topic-id> <channel-id>`.
fn child_compact_commit(root: &Path) -> ! {
    let (channel, topics) = child_pair(root);
    let head = created_channel(channel.create_channel(create_channel_request()));
    let topic = created_topic(&topics, head.channel_id);
    subscribed(&topics, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    published(&topics, topic.topic_id, 0xD1, b"alpha", PUBLISH_ONE_AT_MS);
    published(&topics, topic.topic_id, 0xD2, b"beta", PUBLISH_TWO_AT_MS);
    published(&topics, topic.topic_id, 0xD3, b"gamma", PUBLISH_THREE_AT_MS);
    subscribed(&topics, topic.topic_id, 2, SUBSCRIBER_B_AT_MS);
    assert!(matches!(
        topics.advance(advance_request(topic.topic_id, 1, 2, ADVANCED_AT_MS)),
        Ok(AdvanceDecision::Advanced(_))
    ));
    channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: ACKED_AT_MS,
        })
        .expect("child ack");
    assert!(matches!(
        topics.compact(topic.topic_id, 9),
        Ok(TopicCompactDecision::Trimmed(_))
    ));
    announce(&format!(
        "READY {} {}",
        hex_encode(topic.topic_id.as_bytes()),
        hex_encode(topic.channel_id.as_bytes()),
    ));
    let _keeper = (channel, topics);
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// WAL tail truncation (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Returns `(page_size, frame_size, frame_count)` of a WAL image.
fn wal_frame_layout(wal: &[u8]) -> (usize, usize, usize) {
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    (page_size, frame_size, frame_count)
}

/// Indices of the frames that carry a commit marker (a non-zero
/// database-size-in-pages field at frame-header offset 4..8 — the last
/// frame of each committed transaction).
fn commit_frames(wal: &[u8]) -> Vec<usize> {
    let (_, frame_size, frame_count) = wal_frame_layout(wal);
    (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 4..start + 8].try_into().expect("commit field")) != 0
        })
        .collect()
}

/// Every cut offset that truncates the WAL inside the final `txs_from_end+1`
/// transactions' frame span (from the end of the last surviving commit to
/// the last commit frame inclusive): frame boundaries, half-frame points and
/// last-byte points. `txs_from_end == 0` cuts away the last transaction
/// (torn or whole); `txs_from_end == 1` cuts away the last two.
fn tail_tx_cuts(wal: &[u8], txs_from_end: usize) -> Vec<usize> {
    let (_, frame_size, _) = wal_frame_layout(wal);
    let commits = commit_frames(wal);
    assert!(
        commits.len() >= 2 + txs_from_end,
        "fixture must have enough committed transactions"
    );
    let keep = commits[commits.len() - 2 - txs_from_end];
    let last = commits[commits.len() - 1];
    let mut cuts = vec![32 + (keep + 1) * frame_size];
    for index in (keep + 1)..=last {
        let start = 32 + index * frame_size;
        cuts.push(start);
        cuts.push(start + frame_size / 2);
        cuts.push(start + frame_size - 1);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.retain(|cut| *cut < wal.len());
    cuts
}

/// The on-disk state a killed child leaves behind (both authorities),
/// restorable per sweep iteration so every torn-tail cut starts from
/// identical bytes.
struct FixtureSnapshot {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl FixtureSnapshot {
    fn capture(database: &Path) -> Self {
        Self {
            database: fs::read(database).expect("read database"),
            wal: fs::read(sibling_path(database, "-wal")).expect("read wal"),
        }
    }

    /// Restores the database, rewrites the WAL truncated to `cut` (or in
    /// full for `None`), and drops the stale wal-index.
    fn restore(&self, database: &Path, cut: Option<usize>) {
        fs::write(database, &self.database).expect("restore database");
        let wal = match cut {
            Some(cut) => &self.wal[..cut],
            None => &self.wal[..],
        };
        fs::write(sibling_path(database, "-wal"), wal).expect("restore wal");
        let _ = fs::remove_file(sibling_path(database, "-shm"));
    }
}

/// Both authorities' snapshots for the cross-authority torn-tail sweeps.
struct PairSnapshot {
    topic: FixtureSnapshot,
    channel: FixtureSnapshot,
}

impl PairSnapshot {
    fn capture(base: &Path) -> Self {
        Self {
            topic: FixtureSnapshot::capture(&topic_database(base)),
            channel: FixtureSnapshot::capture(&channel_database(base)),
        }
    }

    fn restore(&self, base: &Path, topic_cut: Option<usize>, channel_cut: Option<usize>) {
        self.topic.restore(&topic_database(base), topic_cut);
        self.channel.restore(&channel_database(base), channel_cut);
    }
}

// ---------------------------------------------------------------------------
// W1: pre-commit IOERR fails typed and converges
// ---------------------------------------------------------------------------

/// W1（`create_topic`）：`FailWritesAfter { 0, IoErr }` 注入 `create_topic` 单事务
/// （owner 读back后的 topic head 插入）提交的 WAL 写入 →
/// `TopicAuthorityError::Sqlite` 显式失败（错误链含 I/O 条件）；重开后三表零
/// 行（schema 前缀保留、topic 完全不可见 `TopicNotFound`）、integrity ok；
/// disarm 后同一请求重做 → `Created`；重开后 (1,0,0) 恰好一行、同请求重放
/// 逐字节 `Replayed`。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_create_topic_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-create");
    let root = TestRoot::new("ioerr-create");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .create_topic(create_topic_request(head.channel_id))
        .expect_err("create_topic must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_topic_counts(root.base(), [0, 0, 0]);

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [0, 0, 0]);
    assert!(matches!(
        recovered.inspect_topic(nlos_topic::TopicId::from_bytes([0x00; 16])),
        Err(TopicAuthorityError::TopicNotFound(_))
    ));
    assert_integrity(&topic_database(root.base()));

    let record = created_topic(&recovered, head.channel_id);
    assert_eq!(record.channel_generation, Generation::INITIAL);
    drop(recovered);
    let verified = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_eq!(
        verified
            .inspect_topic(record.topic_id)
            .expect("topic head after redo"),
        record
    );
    assert!(matches!(
        verified.create_topic(create_topic_request(head.channel_id)),
        Ok(TopicDecision::Replayed(_))
    ));
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_integrity_both(root.base());
}

/// W1（subscribe）：topic 前缀先行落盘，`FailWritesAfter { 0, IoErr }` 注入
/// subscribe 单事务（subscription 行 + active 计数 CAS）提交写入 → typed
/// `Sqlite` 失败；重开后 (1,0,0) 保持前缀；disarm 后同请求重做 →
/// `Subscribed`；重开后重放逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_subscribe_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-subscribe");
    let root = TestRoot::new("ioerr-subscribe");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .subscribe(subscribe_request(topic.topic_id, 1, SUBSCRIBED_AT_MS))
        .expect_err("subscribe must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_topic_counts(root.base(), [1, 0, 0]);

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_integrity(&topic_database(root.base()));

    let record = subscribed(&recovered, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    assert_eq!(record.cursor, 0);
    assert!(record.active);
    drop(recovered);
    let verified = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 1, 0]);
    assert_eq!(
        verified
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("subscription after redo"),
        record
    );
    assert_eq!(
        verified
            .inspect_topic(topic.topic_id)
            .expect("active count after redo")
            .active_subscriptions,
        1
    );
    assert!(matches!(
        verified.subscribe(subscribe_request(topic.topic_id, 1, 9_999)),
        Ok(SubscribeDecision::Replayed(_))
    ));
    assert_topic_counts(root.base(), [1, 1, 0]);
    assert_integrity_both(root.base());
}

/// W1（advance）：publish 前缀先行落盘，`FailWritesAfter { 0, IoErr }` 注入
/// advance 单事务（per-subscriber cursor CAS UPDATE）提交写入 → typed
/// `Sqlite` 失败；重开后游标仍为旧值 0（raw 行级断言）；disarm 后同请求重做
/// → `Advanced` cursor=1；重开后同请求重放携带原时间戳逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_advance_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-advance");
    let root = TestRoot::new("ioerr-advance");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);
    subscribed(&authority, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    published(
        &authority,
        topic.topic_id,
        0xD1,
        b"alpha",
        PUBLISH_ONE_AT_MS,
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .advance(advance_request(topic.topic_id, 1, 1, ADVANCED_AT_MS))
        .expect_err("advance must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_topic_counts(root.base(), [1, 1, 1]);

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen_topics(root.base(), &channel);
    assert_eq!(
        raw_subscriber_cursor(root.base(), &topic.topic_id, 1),
        0,
        "cursor must stay at the old value after the failed commit"
    );
    assert_integrity(&topic_database(root.base()));

    assert!(matches!(
        recovered.advance(advance_request(topic.topic_id, 1, 1, ADVANCED_AT_MS)),
        Ok(AdvanceDecision::Advanced(_))
    ));
    drop(recovered);
    let verified = reopen_topics(root.base(), &channel);
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 1), 1);
    assert_eq!(
        verified
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("subscription after redo")
            .cursor,
        1
    );
    assert!(matches!(
        verified.advance(advance_request(topic.topic_id, 1, 1, 9_999)),
        Ok(AdvanceDecision::Replayed(_))
    ));
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 1), 1);
    assert_integrity_both(root.base());
}

/// W1（publish）：`FailWritesAfter { 0, IoErr }` 注入 publish 第一步 topic 事
/// 务（`PENDING_ENQUEUE` 行落盘）的提交写入 → typed `Sqlite` 失败且 **channel
/// 侧零写入**（verify-then-commit 的验证先于 enqueue：队列零条目）；重开后
/// (1,0,0)；disarm 后同请求重做 → `Published` ENQUEUED seq=1；重开后重放逐
/// 字节相等、channel 恰好一条。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_publish_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-publish");
    let root = TestRoot::new("ioerr-publish");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .publish(publish_request(
            topic.topic_id,
            0xD1,
            b"alpha",
            PUBLISH_ONE_AT_MS,
        ))
        .expect_err("publish must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_eq!(
        raw_channel_entries(root.base()),
        0,
        "channel stays untouched"
    );

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_integrity(&topic_database(root.base()));

    let record = published(
        &recovered,
        topic.topic_id,
        0xD1,
        b"alpha",
        PUBLISH_ONE_AT_MS,
    );
    assert_enqueued(&record, 1, PUBLISH_ONE_AT_MS);
    drop(recovered);
    let verified = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 1]);
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
    assert_eq!(
        replayed_publish(
            &verified,
            publish_request(topic.topic_id, 0xD1, b"alpha", 9_999),
        ),
        record,
        "replay must be byte-equal"
    );
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_integrity_both(root.base());
}

/// W2：`FailWritesAfter { 0, Full }`（`SQLITE_FULL`）对 `create_topic` 与 publish
/// 两条 topic 侧事务路径同一收敛——typed 失败链含 "full"、零幻影行（publish
/// 场景 channel 仍零条目）；disarm 后重做成功、行恰好一套。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // (a) create_topic under SQLITE_FULL.
    {
        let _sandbox = SandboxCwd::new("full-create");
        let root = TestRoot::new("full-create");
        let channel = reopen_channel(root.base());
        let head = created_channel(channel.create_channel(create_channel_request()));
        let authority = open_topic_fault(root.base(), &channel);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = authority
            .create_topic(create_topic_request(head.channel_id))
            .expect_err("create_topic must fail under injected disk-full");
        assert_sqlite_error_chain(&error, &["full"]);
        assert_topic_counts(root.base(), [0, 0, 0]);

        nlos_store_fault::disarm();
        let topic = created_topic(&authority, head.channel_id);
        drop(authority);
        let verified = reopen_topics(root.base(), &channel);
        assert_topic_counts(root.base(), [1, 0, 0]);
        assert_eq!(verified.inspect_topic(topic.topic_id).expect("head"), topic);
        assert_integrity_both(root.base());
    }

    // (b) publish under SQLITE_FULL: the channel enqueue never runs.
    {
        let _sandbox = SandboxCwd::new("full-publish");
        let root = TestRoot::new("full-publish");
        let channel = reopen_channel(root.base());
        let head = created_channel(channel.create_channel(create_channel_request()));
        let authority = open_topic_fault(root.base(), &channel);
        let topic = created_topic(&authority, head.channel_id);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = authority
            .publish(publish_request(
                topic.topic_id,
                0xD1,
                b"alpha",
                PUBLISH_ONE_AT_MS,
            ))
            .expect_err("publish must fail under injected disk-full");
        assert_sqlite_error_chain(&error, &["full"]);
        assert_topic_counts(root.base(), [1, 0, 0]);
        assert_eq!(raw_channel_entries(root.base()), 0);

        nlos_store_fault::disarm();
        let record = published(
            &authority,
            topic.topic_id,
            0xD1,
            b"alpha",
            PUBLISH_ONE_AT_MS,
        );
        assert_enqueued(&record, 1, PUBLISH_ONE_AT_MS);
        drop(authority);
        let verified = reopen_topics(root.base(), &channel);
        assert_topic_counts(root.base(), [1, 0, 1]);
        assert_eq!(raw_channel_entries(root.base()), 1);
        assert_eq!(
            replayed_publish(
                &verified,
                publish_request(topic.topic_id, 0xD1, b"alpha", 9_999),
            ),
            record
        );
        assert_integrity_both(root.base());
    }
}

// ---------------------------------------------------------------------------
// W3: PowerLossAfter the commit point (create_topic + publish)
// ---------------------------------------------------------------------------

/// W3（publish，`PENDING_ENQUEUE` 窗口重点）：
/// - Phase A（断电不可见方向）：topic 侧经 fault VFS、channel 侧普通 VFS；
///   `PowerLossAfter { 0 }` 下 publish "报告成功"但 topic 两段事务（PENDING
///   插入与 ENQUEUED 提交）全部未落盘，而 channel enqueue **真实落盘**——
///   这是 verify-then-commit 声明的最坏分歧：topic 零 publication 行 +
///   channel 恰好一条 entry。重放同一 key → channel 的 key 域幂等把投递
///   补齐：`Published`（补投收敛）ENQUEUED seq=1 与幻影记录逐字节相等，
///   channel 仍恰好一条、无重复 enqueue；再次重放 → `Replayed` 幂等。
/// - Phase B（提交后 kill-9 可见方向）：子进程完整提交 publish 后被强杀；
///   重开 → publication ENQUEUED、channel 恰好一条 entry；同请求重放 →
///   `Replayed` 逐字节相等且不重复 enqueue。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_publish_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    publish_power_loss_invisible_replay_converges_onto_single_entry();
    publish_kill9_after_commit_visible_replay_is_idempotent();
}

fn publish_power_loss_invisible_replay_converges_onto_single_entry() {
    let _sandbox = SandboxCwd::new("pl-publish");
    let root = TestRoot::new("pl-publish");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = match authority
        .publish(publish_request(
            topic.topic_id,
            0xD1,
            b"alpha",
            PUBLISH_ONE_AT_MS,
        ))
        .expect("power loss drops topic writes silently")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("fresh publish cannot replay"),
    };
    nlos_store_fault::disarm();
    // The surviving topic connection keeps a wal-index referencing frames the
    // disk never saw; it must die first (as a real power loss would kill it)
    // so recovery sees durable bytes alone (fault_injection.rs precedent).
    drop(authority);

    // Topic side wholly invisible; channel side durably enqueued exactly once.
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert!(
        recovered
            .inspect_publications(topic.topic_id)
            .expect("no publications after power loss")
            .is_empty()
    );
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
    assert_eq!(raw_channel_max_sequence(root.base()), 1);
    assert_integrity_both(root.base());

    // Replay converges: the key-scoped channel replay 补投 the ENQUEUED
    // association without a duplicate enqueue, byte-equal to the phantom.
    let converged = match recovered
        .publish(publish_request(
            topic.topic_id,
            0xD1,
            b"alpha",
            PUBLISH_ONE_AT_MS,
        ))
        .expect("replay converges after power loss")
    {
        PublishDecision::Published(record) => record,
        PublishDecision::Replayed(_) => panic!("resumed publication must complete as Published"),
    };
    assert_eq!(
        converged, phantom,
        "converged record must be byte-equal to the silently lost one"
    );
    assert_enqueued(&converged, 1, PUBLISH_ONE_AT_MS);
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);

    assert_eq!(
        replayed_publish(
            &recovered,
            publish_request(topic.topic_id, 0xD1, b"alpha", 9_999),
        ),
        converged,
        "follow-up replay is idempotent"
    );
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_topic_counts(root.base(), [1, 0, 1]);
    assert_integrity_both(root.base());
}

fn publish_kill9_after_commit_visible_replay_is_idempotent() {
    let root = TestRoot::new("kill9-publish");
    let mut child = spawn_child("publish-one-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let channel = reopen_channel(root.base());
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 1]);
    let publications = recovered
        .inspect_publications(ids.topic_id)
        .expect("publication must survive the kill");
    assert_eq!(publications.len(), 1);
    let record = publications.into_iter().next().expect("one publication");
    assert_enqueued(&record, 1, PUBLISH_ONE_AT_MS);
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
    assert_eq!(raw_channel_max_sequence(root.base()), 1);
    assert_integrity_both(root.base());

    // Visible direction: the exact request replays byte-equal and the
    // enqueue is never repeated.
    assert_eq!(
        replayed_publish(
            &recovered,
            publish_request(ids.topic_id, 0xD1, b"alpha", 9_999),
        ),
        record,
        "replay must be byte-equal"
    );
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
    assert_integrity_both(root.base());
}

/// W3（`create_topic`）：
/// - Phase A（断电不可见）：`PowerLossAfter { 0 }` 下 `create_topic` "报告成
///   功"；重开后三表零行、`TopicNotFound`——不是部分可见；同请求重做 →
///   `Created` 与幻影记录逐字节相等（确定性 id/fence/digest）。
/// - Phase B（提交后 kill-9 可见）：子进程完整提交后强杀；重开后恰好一行，
///   与子进程宣告的 id/fence 一致；同请求重放 → `Replayed` 逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_create_topic_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    create_topic_power_loss_invisible_redo_byte_equal();
    create_topic_kill9_after_commit_visible_replay_byte_equal();
}

fn create_topic_power_loss_invisible_redo_byte_equal() {
    let _sandbox = SandboxCwd::new("pl-create");
    let root = TestRoot::new("pl-create");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = match authority
        .create_topic(create_topic_request(head.channel_id))
        .expect("power loss drops writes silently")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("fresh create cannot replay"),
    };
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [0, 0, 0]);
    assert!(matches!(
        recovered.inspect_topic(phantom.topic_id),
        Err(TopicAuthorityError::TopicNotFound(_))
    ));
    assert_integrity(&topic_database(root.base()));

    let redone = match recovered
        .create_topic(create_topic_request(head.channel_id))
        .expect("redo create_topic after power loss")
    {
        TopicDecision::Created(record) => record,
        TopicDecision::Replayed(_) => panic!("redo after power loss must be Created"),
    };
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost record"
    );
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_eq!(
        recovered
            .inspect_topic(phantom.topic_id)
            .expect("head visible after redo"),
        phantom
    );
    assert_integrity_both(root.base());
}

fn create_topic_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("kill9-create");
    let mut child = spawn_child("topic-create-commit", &root);
    let marker = await_marker(&mut child);
    let fence = hex_decode32(marker_part(&marker, 2));
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let channel = reopen_channel(root.base());
    let recovered = reopen_topics(root.base(), &channel);
    assert_topic_counts(root.base(), [1, 0, 0]);
    let record = recovered
        .inspect_topic(ids.topic_id)
        .expect("topic head must survive the kill");
    assert_eq!(record.channel_id, ids.channel_id);
    assert_eq!(record.channel_fencing_token, fence);
    assert_eq!(record.channel_generation, Generation::INITIAL);
    assert_eq!(record.active_subscriptions, 0);
    assert_eq!(record.created_at_ms, TOPIC_CREATED_AT_MS);
    assert_integrity_both(root.base());

    assert!(matches!(
        recovered.create_topic(create_topic_request(ids.channel_id)),
        Ok(TopicDecision::Replayed(replayed)) if replayed == record
    ));
    assert_topic_counts(root.base(), [1, 0, 0]);
    assert_integrity_both(root.base());
}

// ---------------------------------------------------------------------------
// W4: torn WAL tail (topic create/publish paths + channel publish path)
// ---------------------------------------------------------------------------

/// W4（topic 侧，publish 路径，双向收敛）：子进程提交 subscribe + publish#1 +
/// publish#2（pending2 → enqueued2 两段事务）后被强杀；父进程对 topic WAL 最
/// 后一段事务帧组的每个截断点与再深一段的截断点（合计 ≥5 个代表点）逐一
/// 恢复重开，分支按**观测到的重开状态**归类：
/// - 每个截断点下 pub#1 恒 ENQUEUED seq=1、channel 恒恰好 2 条 entry（topic
///   侧截断不扰动 channel 文件）；
/// - pub#2 重开后**要么整体不可见（可重做）要么 `PENDING_ENQUEUE`（声明的合
///   法中间态）**，绝无其他形态（半行/错绑 sequence 由 decode 校验排除）；
/// - 两个分支同一收敛路径：重放同 key → channel key 域 Replayed 把
///   ENQUEUED 关联补齐（或重建 PENDING 后补齐），结果与可见对照的原始记
///   录**逐字节相等**，channel 仍恰好一条该 key 的 entry；再重放 →
///   `Replayed` 幂等。
/// - 完整恢复（可见方向）对照重放逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_publish_torn_wal_tail_sweep_converges_pending_or_invisible() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-publish");
    let mut child = spawn_child("publish-two-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = PairSnapshot::capture(root.base());

    // Visible control: the untouched WALs recover both publications whole.
    let verified = {
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);
        let publications = topics
            .inspect_publications(ids.topic_id)
            .expect("visible control publications");
        assert_eq!(publications.len(), 2);
        assert_enqueued(&publications[0], 1, PUBLISH_ONE_AT_MS);
        assert_enqueued(&publications[1], 2, PUBLISH_TWO_AT_MS);
        assert_eq!(raw_channel_entries(root.base()), 2);
        assert_eq!(raw_channel_max_sequence(root.base()), 2);
        assert_eq!(
            replayed_publish(&topics, publish_request(ids.topic_id, 0xD2, b"beta", 9_999),),
            publications[1].clone(),
            "visible-control replay is byte-equal"
        );
        publications
    };
    let record_one = verified[0].clone();
    let record_two = verified[1].clone();
    drop(verified);

    let topic_wal = &snapshot.topic.wal;
    let last_span = tail_tx_cuts(topic_wal, 0);
    let deeper_span = tail_tx_cuts(topic_wal, 1);
    let mut all_cuts = [last_span, deeper_span].concat();
    all_cuts.sort_unstable();
    all_cuts.dedup();
    assert!(
        all_cuts.len() >= 5,
        "sweep must cover at least 5 representative cut points, got {all_cuts:?}"
    );

    for cut in all_cuts {
        snapshot.restore(root.base(), Some(cut), None);
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);
        let publications = topics
            .inspect_publications(ids.topic_id)
            .expect("publications after torn tail");
        // pub#1 and the channel side always survive whole.
        assert_eq!(
            publications.first(),
            Some(&record_one),
            "pub#1 must survive every cut byte-equal"
        );
        assert_eq!(raw_channel_entries(root.base()), 2);
        assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
        assert_eq!(raw_entries_for_key(root.base(), 0xD2), 1);
        assert_eq!(raw_channel_max_sequence(root.base()), 2);
        assert_integrity_both(root.base());

        // pub#2 is either wholly invisible or exactly PENDING_ENQUEUE — the
        // declared legal crash window; nothing else may appear. The branch
        // is classified from the observed state (cuts landing inside the
        // final transaction span keep the PENDING row, deeper cuts drop it).
        match publications.len() {
            2 => {
                let pending = &publications[1];
                assert_eq!(pending.status, PublicationStatus::PendingEnqueue);
                assert_eq!(pending.channel_sequence, 0);
                assert_eq!(pending.channel_generation, 0);
                assert_eq!(pending.payload_digest, record_two.payload_digest);
                assert_eq!(pending.published_at_ms, PUBLISH_TWO_AT_MS);
            }
            1 => {}
            other => panic!("pub#2 must vanish whole or stay PENDING, saw {other} rows"),
        }

        // Both branches converge through the same replay path onto the
        // byte-equal original record, with no duplicate enqueue.
        let converged = match topics
            .publish(publish_request(
                ids.topic_id,
                0xD2,
                b"beta",
                PUBLISH_TWO_AT_MS,
            ))
            .expect("replay converges after torn tail")
        {
            PublishDecision::Published(record) => record,
            PublishDecision::Replayed(_) => panic!("torn-tail replay must complete as Published"),
        };
        assert_eq!(converged, record_two, "converged record is byte-equal");
        assert_eq!(raw_channel_entries(root.base()), 2);
        assert_eq!(raw_entries_for_key(root.base(), 0xD2), 1);
        assert_eq!(
            replayed_publish(&topics, publish_request(ids.topic_id, 0xD2, b"beta", 9_999),),
            converged
        );
        let publications = topics
            .inspect_publications(ids.topic_id)
            .expect("publications after convergence");
        assert_eq!(publications, vec![record_one.clone(), record_two.clone()]);
        assert_integrity_both(root.base());
        drop(topics);
        drop(channel);
    }

    // Full restore returns to the visible world and replays byte-equal.
    snapshot.restore(root.base(), None, None);
    let channel = reopen_channel(root.base());
    let verified = reopen_topics(root.base(), &channel);
    assert_eq!(
        verified
            .inspect_publications(ids.topic_id)
            .expect("publications after full restore"),
        vec![record_one, record_two]
    );
    assert_integrity_both(root.base());
}

/// W4（topic 侧，create 路径）：子进程提交 `create_topic` 后被强杀，topic WAL
/// 最后一段事务帧组（create 事务）的每个截断点逐一恢复重开 → topic 三表恒
/// 零行、`TopicNotFound`（topic head 整体消失，schema 前缀保留）、integrity
/// ok、channel 侧 create 前缀不受扰动；同请求重做 → `Created` 与可见对照的
/// 记录逐字节相等；完整恢复重放逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_create_topic_torn_wal_tail_discards_whole() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-create");
    let mut child = spawn_child("topic-create-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = FixtureSnapshot::capture(&topic_database(root.base()));

    // Visible control.
    let visible = {
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);
        let record = topics
            .inspect_topic(ids.topic_id)
            .expect("visible control topic");
        assert!(matches!(
            topics.create_topic(create_topic_request(ids.channel_id)),
            Ok(TopicDecision::Replayed(replayed)) if replayed == record
        ));
        record
    };

    let cuts = tail_tx_cuts(&snapshot.wal, 0);
    assert!(
        cuts.len() >= 3,
        "sweep must cover several cut points, got {cuts:?}"
    );
    for cut in cuts {
        snapshot.restore(&topic_database(root.base()), Some(cut));
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);
        assert_topic_counts(root.base(), [0, 0, 0]);
        assert!(matches!(
            topics.inspect_topic(ids.topic_id),
            Err(TopicAuthorityError::TopicNotFound(_))
        ));
        assert_integrity(&topic_database(root.base()));
        assert_eq!(
            raw_count(
                &channel_database(root.base()),
                "SELECT COUNT(*) FROM channels"
            ),
            1,
            "channel prefix is untouched by topic-side truncation"
        );

        // Redo is byte-equal to the visible control record.
        let redone = match topics
            .create_topic(create_topic_request(ids.channel_id))
            .expect("redo create_topic after torn tail")
        {
            TopicDecision::Created(record) => record,
            TopicDecision::Replayed(_) => panic!("redo after torn tail must be Created"),
        };
        assert_eq!(redone, visible, "redo must match the killed transaction");
        assert_topic_counts(root.base(), [1, 0, 0]);
        assert_integrity_both(root.base());
        drop(topics);
        drop(channel);
    }

    snapshot.restore(&topic_database(root.base()), None);
    let channel = reopen_channel(root.base());
    let verified = reopen_topics(root.base(), &channel);
    assert_eq!(
        verified
            .inspect_topic(ids.topic_id)
            .expect("topic after full restore"),
        visible
    );
    assert_integrity_both(root.base());
}

/// W4（channel 侧，publish 路径）：子进程提交两条 publish 后被强杀，channel
/// WAL 最后一段事务帧组（第二条 enqueue：entry + byte 记账）的每个截断点逐
/// 一恢复重开：
/// - channel 侧事务**整体消失**：恰好剩一条 entry（seq=1）、`inspect_queue`
///   交叉校验通过（记账随 entry 同生同灭）、integrity ok；
/// - topic 侧 journal 完整保留 pub#2 的 `ENQUEUED seq=2` 绑定，形成「topic
///   有 publication 行而 channel 无对应 entry」的悬空态——这正是任务声明
///   的既有边界中间态（channel compaction 亦可合法产生同族状态）：它被
///   **fail-closed 检出**（`inspect_publications` →
///   `CorruptRecord("publication sequence exceeds the channel high-water")`），
///   绝无越过该边界的软成功；poll 只服务残存日志（seq=1）；
/// - 同 key 重放 → `Replayed` 逐字节相等（journal 是权威）、channel 零新增
///   entry、无重复 enqueue；
/// - 完整恢复对照：两侧完整、重放幂等。
///
/// **模型边界（文档化，非缺陷钉住）**：该悬空态要求丢掉一个**已 fsync 的
/// channel 提交**，而该提交在 durable 顺序上**先于** topic 的 ENQUEUED 提
/// 交——真实进程崩溃（kill-9 页面缓存存活）与单文件尾部丢失都到不了这个
/// 窗口，只有跨文件的磁盘级 torn-write 模型可以。它是声明的
/// 「无跨权威原子性」边界内的可检测状态（CorruptRecord 硬失败），且静态
/// 推演可知：若此后发生新的 publish，channel 会按 max live sequence 重新
/// 分配 seq=2，journal 中将出现两行绑定同一 seq——此模型边缘记入回执，
/// 不以 `#[ignore]` 钉住（不属于任何真实崩溃窗口的规范违反）。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_channel_torn_wal_tail_discards_enqueue_whole() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-channel");
    let mut child = spawn_child("publish-two-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = PairSnapshot::capture(root.base());

    // Visible control: both sides complete and replay-able.
    {
        snapshot.restore(root.base(), None, None);
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);
        let publications = topics
            .inspect_publications(ids.topic_id)
            .expect("visible control publications");
        assert_eq!(publications.len(), 2);
        assert_enqueued(&publications[1], 2, PUBLISH_TWO_AT_MS);
        assert_eq!(raw_channel_entries(root.base()), 2);
        assert_eq!(
            channel
                .inspect_queue(ids.channel_id)
                .expect("queue control")
                .max_sequence,
            2
        );
        drop(topics);
        drop(channel);
    }

    let cuts = tail_tx_cuts(&snapshot.channel.wal, 0);
    assert!(
        cuts.len() >= 3,
        "sweep must cover several cut points, got {cuts:?}"
    );
    for cut in cuts {
        snapshot.restore(root.base(), None, Some(cut));
        let channel = reopen_channel(root.base());
        let topics = reopen_topics(root.base(), &channel);

        // The channel enqueue transaction vanished whole.
        let queue = channel.inspect_queue(ids.channel_id).expect("queue state");
        assert_eq!(queue.max_sequence, 1);
        assert_eq!(raw_channel_entries(root.base()), 1);
        assert_eq!(raw_entries_for_key(root.base(), 0xD2), 0);
        assert_eq!(raw_channel_max_sequence(root.base()), 1);
        assert_integrity(&channel_database(root.base()));

        // The topic journal keeps both bindings; pub#2 dangles (seq 2 > 1)
        // and every cross-check fails closed — never a soft success.
        assert_topic_counts(root.base(), [1, 1, 2]);
        assert!(matches!(
            topics.inspect_publications(ids.topic_id),
            Err(TopicAuthorityError::CorruptRecord(
                "publication sequence exceeds the channel high-water"
            ))
        ));
        // The surviving log still serves the surviving entry.
        let window = topics
            .poll(ids.topic_id, subscriber(1), 10)
            .expect("poll serves the durable prefix");
        assert_eq!(
            window
                .iter()
                .map(|entry| entry.sequence)
                .collect::<Vec<_>>(),
            [1]
        );

        // The journal is the authority: the same-key replay is idempotent
        // and never re-enqueues over the lost entry.
        assert_eq!(
            replayed_publish(&topics, publish_request(ids.topic_id, 0xD2, b"beta", 9_999),)
                .channel_sequence,
            2
        );
        assert_eq!(raw_channel_entries(root.base()), 1);
        assert_integrity_both(root.base());
        drop(topics);
        drop(channel);
    }

    // Full restore returns to the complete world.
    snapshot.restore(root.base(), None, None);
    let channel = reopen_channel(root.base());
    let verified = reopen_topics(root.base(), &channel);
    assert_eq!(
        verified
            .inspect_publications(ids.topic_id)
            .expect("publications after full restore")
            .len(),
        2
    );
    assert_eq!(raw_channel_entries(root.base()), 2);
    assert_integrity_both(root.base());
}

// ---------------------------------------------------------------------------
// W5: replay storm
// ---------------------------------------------------------------------------

/// W5：publish / subscribe / advance 同 key 各连放 3 次 + 重开后再各放 1 次
/// → 每次返回与原始 durable 记录**逐字节相等**（publish/subscribe 重放携带
/// 请求的新时间戳仍返回原记录；advance 重放返回存储的原时间戳）；原始
/// record 逐字节稳定；恰一套行（publications 1 行、subscriptions 1 行）、
/// channel 恰好一条 entry 无重复；中途冲突请求恒 `IdempotencyConflict` /
/// `InvalidSequence`；两侧 integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_replay_storm_publish_subscribe_advance_no_duplicates() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = reopen_topics(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);

    subscribed(&authority, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    let publication = published(
        &authority,
        topic.topic_id,
        0xD1,
        b"alpha",
        PUBLISH_ONE_AT_MS,
    );
    assert!(matches!(
        authority.advance(advance_request(topic.topic_id, 1, 1, ADVANCED_AT_MS)),
        Ok(AdvanceDecision::Advanced(_))
    ));
    let advanced_receipt = authority
        .advance(advance_request(topic.topic_id, 1, 1, ADVANCED_AT_MS))
        .expect("same-value advance replays the stored receipt");
    let advanced = match advanced_receipt {
        AdvanceDecision::Replayed(receipt) => receipt,
        AdvanceDecision::Advanced(_) => panic!("expected Replayed, got Advanced"),
    };
    // An active-key subscribe replay returns the CURRENT durable row (the
    // advance above moved its cursor), captured here as the replay truth.
    let subscription = authority
        .inspect_subscription(topic.topic_id, subscriber(1))
        .expect("current subscription row");
    assert_eq!(subscription.cursor, 1);

    for _ in 0..3 {
        assert_eq!(
            replayed_publish(
                &authority,
                publish_request(topic.topic_id, 0xD1, b"alpha", 9_999),
            ),
            publication,
            "every storm replay is byte-equal"
        );
        assert!(matches!(
            authority.subscribe(subscribe_request(topic.topic_id, 1, 9_999)),
            Ok(SubscribeDecision::Replayed(record)) if record == subscription
        ));
        assert!(matches!(
            authority.advance(advance_request(topic.topic_id, 1, 1, 9_999)),
            Ok(AdvanceDecision::Replayed(receipt)) if receipt == advanced
        ));
    }
    // Conflicting forms keep failing mid-storm.
    assert!(matches!(
        authority.publish(publish_request(topic.topic_id, 0xD1, b"drift", 9_999)),
        Err(TopicAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        authority.advance(advance_request(topic.topic_id, 1, 0, 9_999)),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));

    drop(authority);
    let verified = reopen_topics(root.base(), &channel);
    assert_eq!(
        replayed_publish(
            &verified,
            publish_request(topic.topic_id, 0xD1, b"alpha", 9_999),
        ),
        publication,
        "replay after reopen stays byte-equal"
    );
    assert!(matches!(
        verified.subscribe(subscribe_request(topic.topic_id, 1, 9_999)),
        Ok(SubscribeDecision::Replayed(record)) if record == subscription
    ));
    assert!(matches!(
        verified.advance(advance_request(topic.topic_id, 1, 1, 9_999)),
        Ok(AdvanceDecision::Replayed(receipt)) if receipt == advanced
    ));

    assert_topic_counts(root.base(), [1, 1, 1]);
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 1), 1);
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_entries_for_key(root.base(), 0xD1), 1);
    assert_integrity_both(root.base());
}

// ---------------------------------------------------------------------------
// W6: advance CAS kill-window
// ---------------------------------------------------------------------------

/// W6（advance CAS kill-window）：advance 单事务在 cursor CAS 持久化前后崩
/// 溃只有两个合法终点——Phase A（`PowerLossAfter { 0 }`，CAS 未持久化）游标
/// 处于**旧值 0**（可重做），raw 行级与 API 双重断言；越界/回退请求恒
/// `InvalidSequence`；重做恰一次跨过 CAS → 游标 2、回执与幻影逐字节相等；
/// 之后同请求重放携带原时间戳逐字节相等；旁路订阅者 B 的游标全程不动
/// （per-subscriber CAS 隔离）。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_advance_cas_kill_window_has_no_intermediate_cursor() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("cas");
    let root = TestRoot::new("cas");
    let channel = reopen_channel(root.base());
    let head = created_channel(channel.create_channel(create_channel_request()));
    let authority = open_topic_fault(root.base(), &channel);
    let topic = created_topic(&authority, head.channel_id);
    subscribed(&authority, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    subscribed(&authority, topic.topic_id, 2, SUBSCRIBER_B_AT_MS);
    published(
        &authority,
        topic.topic_id,
        0xD1,
        b"alpha",
        PUBLISH_ONE_AT_MS,
    );
    published(&authority, topic.topic_id, 0xD2, b"beta", PUBLISH_TWO_AT_MS);

    // Crash before the CAS UPDATE is durable: the cursor stays at the old
    // value; the phantom receipt is what a durable redo must reproduce.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = match authority
        .advance(advance_request(topic.topic_id, 1, 2, ADVANCED_AT_MS))
        .expect("power loss drops writes silently")
    {
        AdvanceDecision::Advanced(receipt) => receipt,
        AdvanceDecision::Replayed(_) => panic!("fresh advance cannot replay"),
    };
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen_topics(root.base(), &channel);
    assert_eq!(
        raw_subscriber_cursor(root.base(), &topic.topic_id, 1),
        0,
        "kill before CAS durable must leave the cursor at the old value"
    );
    assert_eq!(
        recovered
            .inspect_subscription(topic.topic_id, subscriber(1))
            .expect("subscription survives")
            .cursor,
        0
    );
    // Out-of-range and regressing forms keep failing closed in this state.
    assert!(matches!(
        recovered.advance(advance_request(topic.topic_id, 1, 3, 9_999)),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));
    // The sibling subscriber (subscribed before the publications, cursor 0)
    // never moves.
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 2), 0);

    // Redo crosses the CAS exactly once; the receipt is byte-equal.
    let redone = match recovered
        .advance(advance_request(topic.topic_id, 1, 2, ADVANCED_AT_MS))
        .expect("redo advance across the kill window")
    {
        AdvanceDecision::Advanced(receipt) => receipt,
        AdvanceDecision::Replayed(_) => panic!("redo must be Advanced"),
    };
    assert_eq!(redone, phantom);
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 1), 2);

    // After the CAS is durable the same request replays the stored receipt.
    assert!(matches!(
        recovered.advance(advance_request(topic.topic_id, 1, 2, 9_999)),
        Ok(AdvanceDecision::Replayed(receipt)) if receipt == redone
    ));
    assert!(matches!(
        recovered.advance(advance_request(topic.topic_id, 1, 1, 9_999)),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));
    assert!(matches!(
        recovered.advance(advance_request(topic.topic_id, 1, 3, 9_999)),
        Err(TopicAuthorityError::InvalidSequence(_))
    ));
    assert_eq!(raw_subscriber_cursor(root.base(), &topic.topic_id, 2), 0);
    assert_integrity_both(root.base());
}

// ---------------------------------------------------------------------------
// W7: compact kill-window and the min-live-cursor clamp
// ---------------------------------------------------------------------------

/// 期望的 compact 回执常量（fixture 的 min live cursor = A 的 2）。
fn expected_compact_receipt(ids: &IdMarker) -> TopicCompactReceipt {
    TopicCompactReceipt {
        topic_id: ids.topic_id,
        channel_id: ids.channel_id,
        effective_trim_high_water: 2,
        channel: CompactReceipt {
            channel_id: ids.channel_id,
            trim_high_water: 2,
        },
    }
}

/// W7：topic `compact` 的唯一持久写入是 channel trim 事务，故 channel 经
/// fault VFS、topic 普通 VFS：
/// - Phase A（trim 事务崩溃）：`PowerLossAfter { 0 }` 下 compact "报告
///   Trimmed" 但 trim 从未落盘；重开后 channel trim 水位仍 0、三 条 entry
///   全在、两个订阅者游标原值（A=2、B=3）、`compact_bound` 仍 2（min
///   live-cursor 语义未被破坏）；重做 → 真实 `Trimmed` effective=2，恰好
///   删除 seq≤2 的 entry、seq=3 保留（trim 不越过活跃订阅者游标——A 的游标
///   为 2，故 3 不可触碰）；同请求重放 → `Replayed` 与回执常量逐字段相等。
/// - Phase B（kill-9 于 trim 提交后）：子进程完整提交 compact 后强杀；重开
///   → trim 水位 2 持久、恰好一条 entry（seq=3）、订阅者游标原值；重放 →
///   `Replayed` 与期望回执逐字段相等；bound 仍 2。
#[test]
#[allow(clippy::too_many_lines)]
fn topic_fault_compact_crash_window_preserves_min_live_cursor_clamp() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    compact_power_loss_before_channel_trim_redo_converges();
    compact_kill9_after_channel_trim_replays_byte_equal();
}

fn compact_prefix(channel: &ChannelAuthority, topics: &TopicAuthority) -> nlos_topic::TopicId {
    let head = created_channel(channel.create_channel(create_channel_request()));
    let topic = created_topic(topics, head.channel_id);
    subscribed(topics, topic.topic_id, 1, SUBSCRIBED_AT_MS);
    published(topics, topic.topic_id, 0xD1, b"alpha", PUBLISH_ONE_AT_MS);
    published(topics, topic.topic_id, 0xD2, b"beta", PUBLISH_TWO_AT_MS);
    published(topics, topic.topic_id, 0xD3, b"gamma", PUBLISH_THREE_AT_MS);
    // B joins after three publications (cursor 3); A stays behind at 0.
    subscribed(topics, topic.topic_id, 2, SUBSCRIBER_B_AT_MS);
    assert!(matches!(
        topics.advance(advance_request(topic.topic_id, 1, 2, ADVANCED_AT_MS)),
        Ok(AdvanceDecision::Advanced(_))
    ));
    channel
        .ack(AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: 3,
            acked_at_ms: ACKED_AT_MS,
        })
        .expect("owner ack to 3");
    topic.topic_id
}

fn assert_compact_prefix_state(
    base: &Path,
    topic_id: &nlos_topic::TopicId,
    topics: &TopicAuthority,
) {
    assert_eq!(raw_channel_entries(base), 3);
    assert_eq!(raw_channel_water(base, "trim_high_water"), 0);
    assert_eq!(raw_channel_water(base, "consume_high_water"), 3);
    assert_eq!(raw_subscriber_cursor(base, topic_id, 1), 2);
    assert_eq!(raw_subscriber_cursor(base, topic_id, 2), 3);
    assert_eq!(
        topics.compact_bound(*topic_id).expect("bound clamped"),
        2,
        "min live cursor must keep clamping the bound"
    );
}

fn compact_power_loss_before_channel_trim_redo_converges() {
    let _sandbox = SandboxCwd::new("pl-compact");
    let root = TestRoot::new("pl-compact");
    let channel = open_channel_fault(root.base());
    let authority = reopen_topics(root.base(), &channel);
    let topic_id = compact_prefix(&channel, &authority);
    assert_compact_prefix_state(root.base(), &topic_id, &authority);

    // Crash before the channel trim is durable: the compact "succeeds" but
    // nothing reached the disk.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = match authority
        .compact(topic_id, 9)
        .expect("power loss drops the trim silently")
    {
        TopicCompactDecision::Trimmed(receipt) => receipt,
        TopicCompactDecision::Replayed(_) => panic!("fresh compact cannot replay"),
    };
    nlos_store_fault::disarm();
    assert_eq!(phantom.effective_trim_high_water, 2);
    // The poisoned channel connection dies first (power-loss precedent).
    drop(authority);
    drop(channel);

    let channel = reopen_channel(root.base());
    let recovered = reopen_topics(root.base(), &channel);
    // Nothing was trimmed; every cursor and the bound are intact.
    assert_compact_prefix_state(root.base(), &topic_id, &recovered);
    assert_integrity_both(root.base());

    // Redo converges: the trim runs for real, clamped at the min live
    // cursor 2 — entries 1..2 are released, entry 3 (beyond A's cursor)
    // is untouchable.
    let redone = match recovered
        .compact(topic_id, 9)
        .expect("redo compact after power loss")
    {
        TopicCompactDecision::Trimmed(receipt) => receipt,
        TopicCompactDecision::Replayed(_) => panic!("redo must be Trimmed"),
    };
    assert_eq!(redone, phantom);
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_channel_max_sequence(root.base()), 3);
    assert_eq!(raw_channel_water(root.base(), "trim_high_water"), 2);
    assert_eq!(raw_subscriber_cursor(root.base(), &topic_id, 1), 2);
    assert_eq!(
        recovered.compact_bound(topic_id).expect("bound after trim"),
        2
    );
    // Same effective watermark replays the durable receipt.
    assert!(matches!(
        recovered.compact(topic_id, 9),
        Ok(TopicCompactDecision::Replayed(receipt)) if receipt == redone
    ));
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_integrity_both(root.base());
}

fn compact_kill9_after_channel_trim_replays_byte_equal() {
    let root = TestRoot::new("kill9-compact");
    let mut child = spawn_child("compact-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_id_marker(&marker);
    kill_and_reap(&mut child);

    let channel = reopen_channel(root.base());
    let recovered = reopen_topics(root.base(), &channel);
    // The trim is durable and clamped at the min live cursor.
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_channel_max_sequence(root.base()), 3);
    assert_eq!(raw_channel_water(root.base(), "trim_high_water"), 2);
    assert_eq!(raw_channel_water(root.base(), "consume_high_water"), 3);
    assert_eq!(raw_subscriber_cursor(root.base(), &ids.topic_id, 1), 2);
    assert_eq!(raw_subscriber_cursor(root.base(), &ids.topic_id, 2), 3);
    assert_eq!(
        recovered
            .compact_bound(ids.topic_id)
            .expect("bound after durable trim"),
        2
    );
    assert_integrity_both(root.base());

    // Replaying the same trim target returns the durable receipt.
    assert_eq!(
        recovered.compact(ids.topic_id, 9).expect("compact replay"),
        TopicCompactDecision::Replayed(expected_compact_receipt(&ids))
    );
    assert_eq!(raw_channel_entries(root.base()), 1);
    // A higher target stays clamped: no trim may cross the live cursor.
    assert!(matches!(
        recovered.compact(ids.topic_id, 99),
        Ok(TopicCompactDecision::Replayed(receipt)) if receipt.effective_trim_high_water == 2
    ));
    assert_eq!(raw_channel_entries(root.base()), 1);
    assert_eq!(raw_subscriber_cursor(root.base(), &ids.topic_id, 1), 2);
    assert_integrity_both(root.base());
}
