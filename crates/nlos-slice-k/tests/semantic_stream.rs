//! W50 semantic notification stream tests: the production consumer of the
//! `semantic_outbox` rows every admission writes. The pump bootstraps a
//! well-known system channel/topic/subscription idempotently, drains the
//! pending prefix into 73-byte topic envelopes, acknowledges the outbox
//! under the clock's read-only wall high-water, and follows/compacts the
//! system cursor so the bounded channel capacity is released.
//!
//! Fixture discipline mirrors `semantic_writer_bridge.rs` (one package
//! identity, single `executable` entry); this file's idempotency/clock band
//! is `0x75 + 50..57`, disjoint from every documented slice-k helper band.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::Signer;
use nlos_artifact::{
    ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest, PackageManifestEntry,
    ProvenanceSourceTriple, PutRevisionRequest, SignedPackage, VerifyPackageRequest,
    derive_artifact_id, package_manifest_message,
};
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_semantic::SemanticPayloadIdentity;
use nlos_slice_k::{
    OperationReceiptFact, Publisher, SEMANTIC_STREAM_ENVELOPE_BYTES, SemanticStreamConfig,
    SemanticStreamState, SliceKError, SliceKRuntime, execute_application_payload, seeded_key,
    semantic_stream_envelope, semantic_stream_publish_key,
};
use nlos_topic::{PublishRequest, SubscribeRequest, SubscriberKey};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, Generation, OperationId, PackageId, ReceiptId,
    SemanticEventId,
};

const PACKAGE_BYTES: [u8; 16] = [
    0x31, 0x0a, 0xd4, 0x77, 0xe2, 0x1b, 0x64, 0x9c, 0x53, 0x06, 0x88, 0xbf, 0x17, 0x42, 0xaa, 0x58,
];
const ENTRY: &str = "sample-driver";
const SEED: u8 = 0x75;

const PAYLOAD_A: &[u8] = b"semantic stream payload A - executable bytes v1\n";

/// How long a wait loop polls before failing the test.
const WAIT: Duration = Duration::from_secs(8);

struct TempDir {
    root: std::path::PathBuf,
}

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempDir {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-semantic-stream-{label}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove semantic stream temp root: {error}"),
        }
    }
}

fn package_id() -> PackageId {
    PackageId::from_bytes(PACKAGE_BYTES)
}

/// Polls `check` every 20ms until it holds or [`WAIT`] elapses.
fn wait_until(check: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    let mut check = check;
    while start.elapsed() < WAIT {
        if check() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    check()
}

fn verified_payload_package(
    runtime: &SliceKRuntime,
    signer: &Publisher,
    version: u64,
    payload: &[u8],
    key_offset: u8,
    clock_offset: u8,
) -> ReceiptId {
    let artifact_id = ArtifactId::from_bytes(derive_artifact_id(package_id(), version, ENTRY));
    let at_ms = runtime
        .wall_now_ms(seeded_key(SEED, clock_offset))
        .expect("entry publish clock");
    runtime
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: seeded_key(SEED, key_offset),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: at_ms,
        })
        .expect("create entry artifact");
    runtime
        .artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: payload,
            created_at_ms: at_ms,
            provenance: ProvenanceSourceTriple {
                source_a: *artifact_id.as_bytes(),
                source_b: PACKAGE_BYTES,
                source_digest: ContentDigest::of_bytes(payload),
            },
        })
        .expect("put entry revision");
    let manifest = PackageManifest {
        package_id: package_id(),
        version,
        entries: vec![PackageManifestEntry {
            name: ENTRY.to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(payload),
            role: PackageEntryRole::Executable,
        }],
    };
    let envelope = SignedPackage {
        signature: signer
            .signing
            .sign(&package_manifest_message(&manifest))
            .to_bytes(),
        signer: signer.principal_id,
        manifest,
    };
    let decision = runtime
        .artifacts
        .verify_package(
            &runtime.identity,
            VerifyPackageRequest {
                signed: &envelope,
                idempotency_key: seeded_key(SEED, key_offset + 1),
                verified_at_ms: runtime
                    .wall_now_ms(seeded_key(SEED, clock_offset + 1))
                    .expect("verify clock"),
            },
        )
        .expect("verify payload package");
    decision.receipt().receipt_id
}

/// The W49 bridge admits exactly one assertion per distinct operation
/// tuple; driving the bridge with distinct synthetic operation identities
/// is the lightest production-path way to grow the pending prefix.
fn admit_receipt_facts(
    runtime: &SliceKRuntime,
    application_id: ApplicationId,
    count: u8,
) -> Vec<SemanticEventId> {
    (1..=count)
        .map(|index| {
            let receipt = ReceiptId::from_bytes([index; 16]);
            let fact = OperationReceiptFact {
                application_id,
                package_id: package_id(),
                package_version: 1,
                installation_generation: Generation::INITIAL,
                entry_name: ENTRY,
                artifact_id: ArtifactId::from_bytes([index; 16]),
                payload_revision: u64::from(index),
                payload_digest: [index; 32],
                payload_size_bytes: u64::from(index) * 8,
                operation_id: OperationId::from_bytes([index; 16]),
                operation_generation: Generation::INITIAL,
                callback_id: CallbackId::from_bytes([index; 16]),
                outcome: CompletionOutcome::Completed {
                    receipt_id: receipt,
                },
                terminal_state: OperationState::Completed {
                    receipt_id: receipt,
                },
            };
            let append = runtime
                .semantic_writer()
                .append_operation_receipt(runtime, &fact)
                .expect("bridge admits the receipt fact");
            assert!(!append.replayed);
            append.event_id
        })
        .collect()
}

/// Subscribes an independent observer before the pump starts, so the
/// observer's cursor sits below every pump publication and its (lagging)
/// cursor also pins the compact bound — the deterministic way to watch
/// envelopes the pump itself would consume and trim.
fn subscribe_observer(runtime: &SliceKRuntime, topic_id: nlos_topic::TopicId) {
    runtime
        .topics
        .subscribe(SubscribeRequest {
            topic_id,
            subscriber_key: observer_key(),
            subscribed_at_ms: runtime
                .wall_now_ms(seeded_key(SEED, 57))
                .expect("observer subscribe clock"),
        })
        .expect("observer subscribes to the system topic");
}

fn observer_key() -> SubscriberKey {
    SubscriberKey::from_bytes([0xa5; 16])
}

fn pending_count(runtime: &SliceKRuntime) -> usize {
    runtime
        .semantic()
        .list_pending_outbox(1000)
        .expect("pending enumeration")
        .len()
}

/// Parses one envelope payload per the documented 73-byte layout.
fn parse_envelope(payload: &[u8]) -> (u8, u64, SemanticEventId, [u8; 32]) {
    assert_eq!(payload.len(), SEMANTIC_STREAM_ENVELOPE_BYTES);
    let mut log_seq_bytes = [0_u8; 8];
    log_seq_bytes.copy_from_slice(&payload[1..9]);
    let mut event_id_bytes = [0_u8; 32];
    event_id_bytes.copy_from_slice(&payload[9..41]);
    let mut digest = [0_u8; 32];
    digest.copy_from_slice(&payload[41..73]);
    (
        payload[0],
        u64::from_be_bytes(log_seq_bytes),
        SemanticEventId::from_bytes(event_id_bytes),
        digest,
    )
}

#[test]
fn pump_delivers_envelopes_and_drains_pending() {
    let dir = TempDir::new("deliver");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let signer = runtime.bootstrap_publisher(SEED).expect("publisher");
    let receipt = verified_payload_package(&runtime, &signer, 1, PAYLOAD_A, 50, 54);
    runtime
        .install_verified_package_by_id(receipt, SEED)
        .expect("install");
    let execution =
        execute_application_payload(&runtime, package_id(), ENTRY).expect("execute payload");
    assert_eq!(pending_count(&runtime), 1, "the admit is pending pre-pump");

    let binding = runtime.bootstrap_semantic_stream().expect("bootstrap");
    subscribe_observer(&runtime, binding.topic_id());
    let started = runtime.start_semantic_stream().expect("start stream");
    assert_eq!(started.topic_id(), binding.topic_id());

    assert!(
        wait_until(|| pending_count(&runtime) == 0),
        "pending prefix must drain; still pending: {}",
        pending_count(&runtime)
    );

    // The observer sees the 73-byte envelope with the exact owner facts.
    let entries = runtime
        .topics
        .poll(binding.topic_id(), observer_key(), 10)
        .expect("observer polls");
    let matched = entries
        .iter()
        .find(|entry| payload_event_id(entry) == execution.semantic_event_id)
        .expect("observer saw the admitted event's envelope");
    let (event_type, log_seq, event_id, digest) = parse_envelope(&matched.payload);
    assert_eq!(event_type, 1, "the bridged event is an assertion");
    assert_eq!(log_seq, execution.semantic_log_seq);
    assert_eq!(event_id, execution.semantic_event_id);
    assert_eq!(
        runtime
            .semantic()
            .inspect_event(execution.semantic_event_id)
            .expect("event readable")
            .payload_identity,
        SemanticPayloadIdentity::AssertionContent(digest),
        "the envelope digest is the event's content digest"
    );

    // The outbox row left the pending prefix through a recorded ack.
    let outbox = runtime
        .semantic()
        .inspect_outbox(execution.semantic_event_id)
        .expect("outbox readable");
    let admitted = runtime
        .semantic()
        .inspect_admission_receipt(execution.semantic_event_id)
        .expect("receipt readable");
    let acknowledged = outbox.acknowledged_at_ms.expect("ack recorded");
    assert!(acknowledged >= admitted.admitted_at_ms);

    let health = runtime
        .semantic_stream_health()
        .expect("pump runs after draining");
    assert_eq!(health.state, SemanticStreamState::Running);
    assert!(health.published >= 1);
    assert!(health.acknowledged >= 1);
    assert!(health.last_error.is_none());
}

fn payload_event_id(entry: &nlos_channel::QueueEntryRecord) -> SemanticEventId {
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(&entry.payload[9..41]);
    SemanticEventId::from_bytes(bytes)
}

#[test]
fn bootstrap_is_idempotent_and_reopen_replays_the_same_identities() {
    let dir = TempDir::new("bootstrap");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let first = runtime
        .bootstrap_semantic_stream()
        .expect("first bootstrap");
    let second = runtime
        .bootstrap_semantic_stream()
        .expect("second bootstrap");
    assert_eq!(first.channel.channel_id, second.channel.channel_id);
    assert_eq!(first.topic_id(), second.topic_id());
    assert_eq!(
        first.subscription.consume_token,
        second.subscription.consume_token
    );

    let public_key = runtime.semantic_writer().public_key();
    drop(runtime);
    let reopened = SliceKRuntime::open(dir.root()).expect("reopen runtime");
    assert_eq!(reopened.semantic_writer().public_key(), public_key);
    let third = reopened
        .bootstrap_semantic_stream()
        .expect("bootstrap after reopen");
    assert_eq!(third.channel.channel_id, first.channel.channel_id);
    assert_eq!(third.topic_id(), first.topic_id());
    assert_eq!(
        third.subscription.consume_token,
        first.subscription.consume_token
    );
}

#[test]
fn crash_window_republish_replays_zero_double_send() {
    let dir = TempDir::new("crash-window");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let application = ApplicationId::from_bytes([0x51; 16]);
    let events = admit_receipt_facts(&runtime, application, 1);
    let binding = runtime.bootstrap_semantic_stream().expect("bootstrap");
    let row = runtime
        .semantic()
        .list_pending_outbox(1)
        .expect("pending row")
        .pop()
        .expect("one pending row");
    assert_eq!(row.event_id, events[0]);

    // Simulate a pump that crashed after publish, before the outbox ack:
    // the same envelope bytes and the same event-derived idempotency key a
    // pump life would have used.
    let published = runtime
        .topics
        .publish(PublishRequest {
            topic_id: binding.topic_id(),
            payload: semantic_stream_envelope(&row),
            idempotency_key: semantic_stream_publish_key(row.event_id),
            published_at_ms: runtime
                .wall_now_ms(seeded_key(SEED, 51))
                .expect("publish clock"),
        })
        .expect("first life publishes")
        .record();
    let first_sequence = published.channel_sequence;
    assert_eq!(first_sequence, 1);

    runtime.start_semantic_stream().expect("restart pump");
    assert!(
        wait_until(|| pending_count(&runtime) == 0),
        "redelivery must acknowledge the row"
    );

    // Zero double-send: the journal holds exactly the original publication
    // and the channel never grew a second entry.
    let journal = runtime
        .topics
        .inspect_publications(binding.topic_id())
        .expect("journal readable");
    let matching = journal
        .iter()
        .filter(|record| record.idempotency_key == semantic_stream_publish_key(row.event_id))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "one publication per event id");
    assert_eq!(matching[0].channel_sequence, first_sequence);
    let queue = runtime
        .channel
        .inspect_queue(binding.channel_id())
        .expect("channel readable");
    assert_eq!(queue.max_sequence, first_sequence);

    let outbox = runtime
        .semantic()
        .inspect_outbox(row.event_id)
        .expect("outbox readable");
    assert!(outbox.acknowledged_at_ms.is_some());
    let health = runtime
        .semantic_stream_health()
        .expect("pump runs after recovery");
    assert!(health.publish_replays >= 1, "the replay must be observed");
}

#[test]
fn idle_pump_cycles_without_writing() {
    let dir = TempDir::new("idle");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let binding = runtime
        .start_semantic_stream_with(SemanticStreamConfig {
            poll_interval: Duration::from_millis(5),
            ..SemanticStreamConfig::default()
        })
        .expect("start idle stream");
    assert!(
        wait_until(|| {
            runtime
                .semantic_stream_health()
                .is_some_and(|health| health.cycles >= 10)
        }),
        "idle cycles must accumulate"
    );

    let health = runtime
        .semantic_stream_health()
        .expect("pump runs while idle");
    assert_eq!(health.state, SemanticStreamState::Running);
    assert_eq!(health.published, 0);
    assert_eq!(health.publish_replays, 0);
    assert_eq!(health.acknowledged, 0);
    assert_eq!(health.advanced_cycles, 0);
    assert_eq!(health.compacted_cycles, 0);
    assert!(health.last_error.is_none());
    assert_eq!(pending_count(&runtime), 0);

    // The stream plane wrote nothing: no publication, no channel entry.
    assert_eq!(
        runtime
            .topics
            .inspect_publications(binding.topic_id())
            .expect("journal readable"),
        Vec::new()
    );
    let queue = runtime
        .channel
        .inspect_queue(binding.channel_id())
        .expect("channel readable");
    assert_eq!(queue.max_sequence, 0);
    assert_eq!(queue.backlog_bytes, 0);
}

#[test]
fn stream_stop_is_idempotent_and_restartable() {
    let dir = TempDir::new("lifecycle");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    runtime.start_semantic_stream().expect("start stream");

    // A second concurrent stream pump fails closed, like the outbox pump.
    let refusal = runtime
        .start_semantic_stream()
        .expect_err("second start must refuse");
    assert!(
        matches!(refusal, SliceKError::Pump(_)),
        "expected a typed pump refusal, got: {refusal}"
    );

    runtime.stop_semantic_stream();
    assert!(
        runtime.semantic_stream_health().is_none(),
        "no pump after stop"
    );
    runtime.stop_semantic_stream();
    assert!(
        runtime.semantic_stream_health().is_none(),
        "stop is idempotent"
    );

    // Restart after a clean stop is a first-class recovery path.
    runtime.start_semantic_stream().expect("restart stream");
    assert!(
        runtime
            .semantic_stream_health()
            .is_some_and(|health| health.state == SemanticStreamState::Running)
    );
}

#[test]
fn cursor_follow_and_compact_release_the_bounded_channel_capacity() {
    const EVENTS: u8 = 12;
    let dir = TempDir::new("capacity");
    let runtime = SliceKRuntime::open(dir.root()).expect("open runtime");
    let application = ApplicationId::from_bytes([0x52; 16]);
    admit_receipt_facts(&runtime, application, EVENTS);
    assert_eq!(pending_count(&runtime), usize::from(EVENTS));

    // A channel that cannot hold the whole backlog at once: four envelopes
    // per batch/cycle, so the pump only finishes if its cursor follow plus
    // compact actually releases capacity every cycle.
    let config = SemanticStreamConfig {
        channel_capacity_bytes: 4 * u64::try_from(SEMANTIC_STREAM_ENVELOPE_BYTES).unwrap(),
        topic_retained_bytes: 4 * u64::try_from(SEMANTIC_STREAM_ENVELOPE_BYTES).unwrap(),
        batch_limit: 4,
        poll_interval: Duration::from_millis(5),
        ..SemanticStreamConfig::default()
    };
    let binding = runtime
        .start_semantic_stream_with(config)
        .expect("start stream under the tight capacity");
    assert!(
        wait_until(|| pending_count(&runtime) == 0),
        "all envelopes must clear the bounded channel; still pending: {}",
        pending_count(&runtime)
    );

    let journal = runtime
        .topics
        .inspect_publications(binding.topic_id())
        .expect("journal readable");
    assert_eq!(journal.len(), usize::from(EVENTS));

    // The system subscriber consumed everything and the compact linkage
    // released the capacity: nothing is held against the bound.
    let queue = runtime
        .channel
        .inspect_queue(binding.channel_id())
        .expect("channel readable");
    assert_eq!(queue.max_sequence, u64::from(EVENTS));
    assert_eq!(queue.consume_high_water, u64::from(EVENTS));
    assert_eq!(queue.trim_high_water, u64::from(EVENTS));
    assert_eq!(queue.backlog_bytes, 0);
}
