use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptRegistrationDecision, AttemptSpec, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-snapshot-receipt-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            PathBuf::from(format!("{}-wal", self.path.display())),
            PathBuf::from(format!("{}-shm", self.path.display())),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove snapshot test database: {error}"),
            }
        }
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x11; 16])
}

fn snapshot(seed: u8) -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes([seed; 16]),
        snapshot_digest: [seed.wrapping_add(1); 32],
        expected_head_commit_seq: 0,
        effect_history_root: empty_effect_history_root(),
        retry_fence_epoch: 0,
    }
}

fn receipt(seed: u8, snapshot: SnapshotBundle) -> TaskSnapshotReceiptSpec {
    TaskSnapshotReceiptSpec {
        task_id: task_id(),
        snapshot,
        receipt_id: ReceiptId::from_bytes([seed; 16]),
        builder_id: [seed.wrapping_add(1); 16],
        builder_version_digest: [seed.wrapping_add(2); 32],
        per_authority_checkpoint_receipts: vec![
            ReceiptId::from_bytes([seed.wrapping_add(3); 16]),
            ReceiptId::from_bytes([seed.wrapping_add(4); 16]),
        ],
        dependency_closure_root: [seed.wrapping_add(5); 32],
        semantic_resolver_digest: [seed.wrapping_add(6); 32],
        canonical_iteration_digest: [seed.wrapping_add(7); 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2_000,
        authority_id: [seed.wrapping_add(8); 16],
        key_id: [seed.wrapping_add(9); 16],
        signature: [seed.wrapping_add(10); 64],
    }
}

fn register_task(authority: &SqliteTaskAuthority) {
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
}

fn attempt(snapshot: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x41; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot,
        cancellation_scope_id: CancellationScopeId::from_bytes([0x42; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x43; 16]),
        registered_at_ms: 3_000,
    }
}

#[test]
fn durable_receipt_replays_and_attempt_keeps_exact_binding_after_restart() {
    let database = TestDatabase::new("replay");
    let snapshot = snapshot(0x21);
    let expected = receipt(0x31, snapshot);
    {
        let authority = SqliteTaskAuthority::open(&database.path).unwrap();
        register_task(&authority);
        assert_eq!(
            authority
                .register_snapshot_receipt(expected.clone())
                .unwrap(),
            expected
        );
        assert_eq!(
            authority
                .register_snapshot_receipt(expected.clone())
                .unwrap(),
            expected
        );
        assert!(matches!(
            authority
                .register_attempt_with_snapshot_receipt(attempt(snapshot), expected.receipt_id)
                .unwrap(),
            AttemptRegistrationDecision::Created(_)
        ));
    }

    let authority = SqliteTaskAuthority::open(&database.path).unwrap();
    assert_eq!(
        authority
            .inspect_snapshot_receipt(task_id(), expected.receipt_id)
            .unwrap(),
        expected
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), TaskAttemptId::from_bytes([0x41; 16]))
            .unwrap()
            .snapshot_receipt_id,
        Some(expected.receipt_id)
    );
    assert!(matches!(
        authority
            .register_attempt_with_snapshot_receipt(attempt(snapshot), expected.receipt_id)
            .unwrap(),
        AttemptRegistrationDecision::Existing(_)
    ));
    assert!(matches!(
        authority.register_attempt(attempt(snapshot)),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

#[test]
fn stale_incomplete_or_conflicting_receipts_fail_closed() {
    let database = TestDatabase::new("invalid");
    let authority = SqliteTaskAuthority::open(&database.path).unwrap();
    register_task(&authority);
    let bundle = snapshot(0x51);

    let mut incomplete = receipt(0x61, bundle);
    incomplete.per_authority_checkpoint_receipts.clear();
    assert!(matches!(
        authority.register_snapshot_receipt(incomplete),
        Err(TaskStoreError::InvalidSnapshotReceipt { .. })
    ));

    let mut stale = receipt(0x62, bundle);
    stale.snapshot.expected_head_commit_seq = 1;
    assert!(matches!(
        authority.register_snapshot_receipt(stale),
        Err(TaskStoreError::InvalidSnapshotReceipt { .. })
    ));

    let expected = receipt(0x63, bundle);
    authority
        .register_snapshot_receipt(expected.clone())
        .unwrap();
    let mut conflicting = expected;
    conflicting.builder_version_digest = [0xff; 32];
    assert!(matches!(
        authority.register_snapshot_receipt(conflicting),
        Err(TaskStoreError::InvalidSnapshotReceipt { .. })
    ));

    let mixed_bundle = snapshot(0x54);
    let mut mixed = receipt(0x64, mixed_bundle);
    mixed.achieved_consistency = SnapshotConsistency::MixedNonSettleable;
    authority.register_snapshot_receipt(mixed.clone()).unwrap();
    assert!(matches!(
        authority.register_attempt_with_snapshot_receipt(attempt(mixed_bundle), mixed.receipt_id,),
        Err(TaskStoreError::InvalidSnapshotReceipt { .. })
    ));

    let different_snapshot = snapshot(0x52);
    assert!(matches!(
        authority.register_attempt_with_snapshot_receipt(
            attempt(different_snapshot),
            ReceiptId::from_bytes([0x63; 16]),
        ),
        Err(TaskStoreError::InvalidSnapshotReceipt { .. })
    ));
}

#[test]
fn receipt_and_checkpoint_rows_are_ddl_immutable() {
    let database = TestDatabase::new("ddl");
    let expected = receipt(0x71, snapshot(0x70));
    {
        let authority = SqliteTaskAuthority::open(&database.path).unwrap();
        register_task(&authority);
        authority
            .register_snapshot_receipt(expected.clone())
            .unwrap();
    }

    let connection = rusqlite::Connection::open(&database.path).unwrap();
    assert!(
        connection
            .execute(
                "UPDATE task_snapshot_receipts SET builder_id = zeroblob(16)",
                [],
            )
            .is_err()
    );
    assert!(
        connection
            .execute(
                "UPDATE task_snapshot_checkpoint_receipts
                 SET checkpoint_receipt_id = zeroblob(16)",
                [],
            )
            .is_err()
    );
}
