use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, FinalizeDecision, FinalizeRequest, ParticipantRegistryState, ParticipantType,
    PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-participant-{}-{}.sqlite3",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.0).unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.0.display(), suffix));
        }
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x11; 16])
}

fn attempt(seed: u8, head: u64, history: [u8; 32]) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([seed; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(1); 16]),
            snapshot_digest: [seed.wrapping_add(2); 32],
            expected_head_commit_seq: head,
            effect_history_root: history,
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(3); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        registered_at_ms: 2_000 + i64::from(seed),
    }
}

fn permit(spec: &AttemptSpec, seed: u8) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 3_000 + i64::from(seed),
    }
}

fn issued(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued permit, got {other:?}"),
    }
}

#[test]
fn task_registration_assigns_durable_self_participant_and_receipts() {
    let database = Database::new();
    let participant_id = {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
        let registry = authority.inspect_participant_registry(task_id()).unwrap();
        assert_eq!(registry.generation, 1);
        assert_eq!(registry.state, ParticipantRegistryState::Open);
        assert_eq!(registry.participants.len(), 1);
        assert_eq!(
            registry.participants[0].participant_type,
            ParticipantType::TaskStore
        );
        registry.participants[0].participant_id
    };

    let authority = database.open();
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .participants[0]
            .participant_id,
        participant_id
    );
    drop(authority);
    let raw = Connection::open(&database.0).unwrap();
    assert!(raw.execute("DELETE FROM task_participants", []).is_err());
    assert!(
        raw.execute(
            "UPDATE task_authority_identity SET participant_id=zeroblob(16)",
            []
        )
        .is_err()
    );
    assert_eq!(
        raw.query_row(
            "SELECT COUNT(*) FROM task_participant_registry_receipts",
            [],
            |row| { row.get::<_, i64>(0) }
        )
        .unwrap(),
        1
    );
}

#[test]
fn permit_atomically_freezes_and_binds_exact_registry_generation_root() {
    let database = Database::new();
    let binding = {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
        let spec = attempt(0x21, 0, empty_effect_history_root());
        authority.register_attempt(spec).unwrap();
        let request = permit(&spec, 0x31);
        let record = issued(authority.request_commit_permit(request.clone()).unwrap());
        let binding = record.participant_registry_binding.unwrap();
        let registry = authority.inspect_participant_registry(task_id()).unwrap();
        assert_eq!(registry.state, ParticipantRegistryState::FrozenForPermit);
        assert_eq!(binding.generation, registry.generation);
        assert_eq!(binding.root, registry.root);
        match authority.request_commit_permit(request).unwrap() {
            PermitDecision::Replayed(replayed) => {
                assert_eq!(replayed.participant_registry_binding, Some(binding));
            }
            other => panic!("expected replay, got {other:?}"),
        }
        binding
    };

    let authority = database.open();
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .root,
        binding.root
    );
}

#[test]
fn next_competition_creates_new_registry_generation_instead_of_unfreezing_old() {
    let database = Database::new();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let first_attempt = attempt(0x41, 0, empty_effect_history_root());
    authority.register_attempt(first_attempt).unwrap();
    let first = issued(
        authority
            .request_commit_permit(permit(&first_attempt, 0x51))
            .unwrap(),
    );
    let first_binding = first.participant_registry_binding.unwrap();
    assert!(matches!(
        authority
            .finalize_commit(FinalizeRequest {
                task_id: task_id(),
                attempt_id: first_attempt.attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: first.permit_id,
                new_effect_history_root: [0x61; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: 5_000,
            })
            .unwrap(),
        FinalizeDecision::Committed(_)
    ));

    let second_attempt = attempt(0x42, 1, [0x61; 32]);
    authority.register_attempt(second_attempt).unwrap();
    let second = issued(
        authority
            .request_commit_permit(permit(&second_attempt, 0x52))
            .unwrap(),
    );
    let second_binding = second.participant_registry_binding.unwrap();
    assert_eq!(second_binding.generation, first_binding.generation + 1);
    assert_ne!(second_binding.root, first_binding.root);
    let current = authority.inspect_participant_registry(task_id()).unwrap();
    assert_eq!(current.generation, 2);
    assert_eq!(current.state, ParticipantRegistryState::FrozenForPermit);
    assert_eq!(current.prior_root, first_binding.root);
    assert_eq!(current.participants.len(), 1);
}
