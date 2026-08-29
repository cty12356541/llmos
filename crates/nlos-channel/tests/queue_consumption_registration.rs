//! Acceptance tests for the ADR-0012 B-PROCESS-002 channel slice: schema v3
//! carries the queue-entry binding columns (idempotent migration; legacy
//! rows decode `None`, never an invented proof) and the consume-side
//! registration entry mirrors `register_wait` — the framework registers the
//! consuming fiber before it consumes, and the authority row carries the
//! registration identity.

use nlos_channel::{
    ChannelAuthority, ChannelAuthorityError, ChannelDecision, ChannelRecord,
    ConsumptionRegistrationDecision, CreateChannelRequest, EnqueueDecision, EnqueueRequest,
    ProducerRegistration, QueueConsumptionRecord, RegisterQueueConsumptionRequest,
};
use nlos_types::{ExecutionFiberId, Generation, IdempotencyKey};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u16) -> IdempotencyKey {
    let mut bytes = [0_u8; 16];
    bytes[..2].copy_from_slice(&seed.to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

fn binding(seed: u8) -> ExecutionFiberId {
    ExecutionFiberId::from_bytes([seed; 16])
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
            "nlos-channel-consume-reg-{label}-{}-{nonce}-{sequence}",
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

fn create(authority: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(200 + u16::from(seed)),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) | ChannelDecision::Replayed(record) => record,
    }
}

fn request(head: &ChannelRecord, seed: u16) -> EnqueueRequest {
    EnqueueRequest {
        channel_id: head.channel_id,
        expected_generation: head.generation,
        expected_fencing_token: head.fencing_token,
        payload: vec![u8::try_from(seed).expect("seed fits u8"); 8],
        idempotency_key: key(seed),
        enqueued_at_ms: 1_500,
    }
}

fn consumption_request(
    channel_id: nlos_types::ChannelId,
    sequence: u64,
    fiber: ExecutionFiberId,
    generation: Generation,
    key_seed: u16,
) -> RegisterQueueConsumptionRequest {
    RegisterQueueConsumptionRequest {
        channel_id,
        sequence,
        binding: fiber,
        fiber_generation: generation,
        idempotency_key: key(key_seed),
        registered_at_ms: u64::from(key_seed) * 10,
    }
}

fn registered(decision: ConsumptionRegistrationDecision) -> QueueConsumptionRecord {
    match decision {
        ConsumptionRegistrationDecision::Registered(record) => record,
        ConsumptionRegistrationDecision::Replayed(_) => panic!("fresh registration cannot replay"),
    }
}

#[test]
fn consume_registration_mirrors_register_wait_and_replays() {
    let root = Root::new("register");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 1);
    let enqueued = match authority.enqueue_registered(
        request(&head, 1),
        ProducerRegistration {
            binding: binding(0x11),
            fiber_generation: Generation::INITIAL,
        },
    ) {
        Ok(EnqueueDecision::Enqueued(entry)) => entry,
        other => panic!("expected Enqueued, got {other:?}"),
    };
    assert_eq!(enqueued.binding, Some(binding(0x11)));
    assert_eq!(enqueued.fiber_generation, Some(Generation::INITIAL));

    let record = registered(
        authority
            .register_queue_consumption(consumption_request(
                head.channel_id,
                enqueued.sequence,
                binding(0x22),
                Generation::INITIAL,
                300,
            ))
            .expect("register consumption"),
    );
    assert_eq!(record.channel_id, head.channel_id);
    assert_eq!(record.sequence, enqueued.sequence);
    assert_eq!(record.binding, binding(0x22));
    assert_eq!(record.fiber_generation, Generation::INITIAL);
    assert_eq!(record.registered_at_ms, 3_000);

    // Exact-key replay: the original registration, nothing re-written.
    let replayed = match authority
        .register_queue_consumption(consumption_request(
            head.channel_id,
            enqueued.sequence,
            binding(0x22),
            Generation::INITIAL,
            300,
        ))
        .expect("replay consumption")
    {
        ConsumptionRegistrationDecision::Replayed(record) => record,
        ConsumptionRegistrationDecision::Registered(_) => {
            panic!("expected Replayed, got Registered")
        }
    };
    assert_eq!(replayed, record);

    // Same identity under a fresh key: the row identity is unique, fail
    // closed (deterministic re-registration, not a duplicate fact).
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            enqueued.sequence,
            binding(0x22),
            Generation::INITIAL,
            301,
        )),
        Err(ChannelAuthorityError::IdempotencyConflict)
    ));

    // Key rebound to a different identity: fail closed.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            enqueued.sequence,
            binding(0x33),
            Generation::INITIAL,
            300,
        )),
        Err(ChannelAuthorityError::IdempotencyConflict)
    ));

    // Stale-incarnation analog at the authority level: the same binding
    // under a different generation is a different identity, never silently
    // accepted.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            enqueued.sequence,
            binding(0x22),
            Generation::INITIAL.checked_next().expect("next"),
            302,
        )),
        Err(ChannelAuthorityError::IdempotencyConflict)
    ));

    // The consume registration survives entry compaction: ack + compact,
    // then the projection read still returns the fact.
    authority
        .ack(nlos_channel::AckRequest {
            channel_id: head.channel_id,
            up_to_sequence: enqueued.sequence,
            acked_at_ms: 2_200,
        })
        .expect("ack");
    authority
        .compact(head.channel_id, enqueued.sequence)
        .expect("compact");
    let listed = authority
        .list_consumptions_for_binding(binding(0x22))
        .expect("list after compaction");
    assert_eq!(listed, vec![record]);
}

#[test]
fn registration_gates_fail_closed_without_side_effects() {
    let root = Root::new("gates");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 2);
    let entry = match authority.enqueue(request(&head, 2)) {
        Ok(EnqueueDecision::Enqueued(entry)) => entry,
        other => panic!("expected Enqueued, got {other:?}"),
    };
    // Legacy (unregistered) enqueue: binding columns decode None.
    assert_eq!(entry.binding, None);
    assert_eq!(entry.fiber_generation, None);

    // Zero binding is not a binding.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            entry.sequence,
            ExecutionFiberId::from_bytes([0; 16]),
            Generation::INITIAL,
            310,
        )),
        Err(ChannelAuthorityError::InvalidBindingRegistration)
    ));
    // Zero sequence.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            0,
            binding(0x44),
            Generation::INITIAL,
            311,
        )),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));
    // Unknown channel.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            nlos_types::ChannelId::from_bytes([9; 16]),
            entry.sequence,
            binding(0x44),
            Generation::INITIAL,
            312,
        )),
        Err(ChannelAuthorityError::ChannelNotFound(_))
    ));
    // Sequence that was never written.
    assert!(matches!(
        authority.register_queue_consumption(consumption_request(
            head.channel_id,
            99,
            binding(0x44),
            Generation::INITIAL,
            313,
        )),
        Err(ChannelAuthorityError::InvalidSequence(_))
    ));

    // Zero side effect: no registration row exists for the binding, and the
    // rejected keys stay free (the retry under the same key succeeds).
    assert!(
        authority
            .list_consumptions_for_binding(binding(0x44))
            .expect("empty listing")
            .is_empty()
    );
    registered(
        authority
            .register_queue_consumption(consumption_request(
                head.channel_id,
                entry.sequence,
                binding(0x44),
                Generation::INITIAL,
                313,
            ))
            .expect("retry after rejections"),
    );
}

#[test]
fn projection_read_isolates_bindings_in_registration_order() {
    let root = Root::new("projection");
    let authority = ChannelAuthority::open(root.path()).expect("open authority");
    let head = create(&authority, 3);
    let first = match authority.enqueue(request(&head, 3)) {
        Ok(EnqueueDecision::Enqueued(entry)) => entry,
        other => panic!("expected Enqueued, got {other:?}"),
    };
    let second = match authority.enqueue(request(&head, 4)) {
        Ok(EnqueueDecision::Enqueued(entry)) => entry,
        other => panic!("expected Enqueued, got {other:?}"),
    };
    registered(
        authority
            .register_queue_consumption(consumption_request(
                head.channel_id,
                second.sequence,
                binding(0x55),
                Generation::INITIAL.checked_next().expect("next"),
                320,
            ))
            .expect("register later"),
    );
    registered(
        authority
            .register_queue_consumption(consumption_request(
                head.channel_id,
                first.sequence,
                binding(0x55),
                Generation::INITIAL,
                321,
            ))
            .expect("register earlier"),
    );
    registered(
        authority
            .register_queue_consumption(consumption_request(
                head.channel_id,
                first.sequence,
                binding(0x66),
                Generation::INITIAL,
                322,
            ))
            .expect("register other binding"),
    );

    let listed = authority
        .list_consumptions_for_binding(binding(0x55))
        .expect("list binding");
    assert_eq!(listed.len(), 2, "other binding's rows never mix in");
    // Registration-time order, not sequence order: the ts-3200 registration
    // (later sequence) precedes the ts-3210 one (earlier sequence).
    assert_eq!(listed[0].sequence, second.sequence);
    assert_eq!(listed[0].registered_at_ms, 3_200);
    assert_eq!(listed[1].sequence, first.sequence);
    assert_eq!(listed[1].registered_at_ms, 3_210);

    // The all-zero value is not a binding: fail closed.
    assert!(matches!(
        authority.list_consumptions_for_binding(ExecutionFiberId::from_bytes([0; 16])),
        Err(ChannelAuthorityError::InvalidBindingRegistration)
    ));
}

#[test]
fn schema_v3_migration_is_idempotent_and_legacy_rows_decode_none() {
    let root = Root::new("migration");
    {
        let authority = ChannelAuthority::open(root.path()).expect("open authority");
        let head = create(&authority, 4);
        authority
            .enqueue(request(&head, 5))
            .expect("legacy enqueue");
    }

    // Legacy row written through the unregistered enqueue decodes None —
    // the pre-v3 row strategy: honestly absent, never an invented proof.
    {
        let authority = ChannelAuthority::open(root.path()).expect("reopen authority");
        let head = create(&authority, 4);
        let window = authority.receive(head.channel_id, 10).expect("receive");
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].binding, None);
        assert_eq!(window[0].fiber_generation, None);
    }

    // Force a re-run of migrate_v3 over an already-v3 schema: the
    // idempotent pre-check must detect the complete schema and only bump
    // the version, leaving every row byte-for-byte intact.
    {
        let raw = Connection::open(root.db()).expect("open raw");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read version");
        assert_eq!(version, 3);
        raw.pragma_update(None, "user_version", 2)
            .expect("rewind version");
    }
    {
        let authority = ChannelAuthority::open(root.path()).expect("reopen after rewind");
        let head = create(&authority, 4);
        let window = authority.receive(head.channel_id, 10).expect("receive");
        assert_eq!(window.len(), 1);
        assert_eq!(window[0].sequence, 1);
        assert_eq!(window[0].payload, vec![5; 8]);
        let raw = Connection::open(root.db()).expect("open raw again");
        let version: i64 = raw
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read version again");
        assert_eq!(version, 3);
    }

    // A partial v3 (registration machinery missing one trigger) fails
    // closed instead of running half a migration.
    {
        let raw = Connection::open(root.db()).expect("open raw");
        raw.execute("DROP TRIGGER channel_queue_consumptions_no_delete", [])
            .expect("simulate partial schema");
        raw.pragma_update(None, "user_version", 2)
            .expect("rewind version");
    }
    assert!(matches!(
        ChannelAuthority::open(root.path()),
        Err(ChannelAuthorityError::CorruptRecord(
            "partial channel queue binding schema"
        ))
    ));
}
