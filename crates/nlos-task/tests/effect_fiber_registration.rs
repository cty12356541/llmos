//! Acceptance tests for the ADR-0012 B-PROCESS-002 effect-plane slice:
//! `register_effect_binding` is the `register_wait` mirror on the effect
//! plane (registration precedes the effect, the slot row and its immutable
//! registration receipt commit in one transaction), and
//! `list_effect_registrations_for_binding` is the binding-side projection
//! read. The write-window matrix converges by idempotent replay per the
//! ADR-0007/0008 precedent: a crash inside the single registration
//! transaction leaves exactly `[absent | complete]`, and the redo either
//! registers fresh or replays byte-equal — never a phantom third state.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, DispatchRequest, EffectBindingDecision, EffectFiberRegistrationRecord,
    EffectPermitDecision, EffectReceiptDecision, FinalizeDecision, FinalizeRequest, IssuedPermit,
    LogicalEffectDescriptor, Outcome, OutcomeRequest, PermitDecision, PermitRecord, PermitRequest,
    PlannedEffect, SlotState, SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError,
    empty_effect_history_root, empty_effect_set_root, idempotency_identity_digest,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, ExecutionFiberId, Generation, IdempotencyKey,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-effect-fiber-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn task_id() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

fn task_spec() -> TaskSpec {
    TaskSpec {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

fn snapshot(head_seq: u64, fence: u64) -> SnapshotBundle {
    let tag = u8::try_from(head_seq).expect("test head fits in u8");
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: if head_seq == 0 {
            empty_effect_history_root()
        } else {
            [0x30 + tag; 32]
        },
        retry_fence_epoch: fence,
    }
}

fn attempt_spec(seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xc0 + seed)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xa0 + seed)),
        registered_at_ms: 2_000,
    }
}

fn descriptor(stable_action_slot: u64) -> LogicalEffectDescriptor {
    LogicalEffectDescriptor {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

fn planned(stable_action_slot: u64, required: bool) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(stable_action_slot),
        required,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn permit_request(spec: &AttemptSpec, seed: u8, effects: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0 + seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
) -> nlos_task::EffectPermitRequest {
    nlos_task::EffectPermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 4_000,
    }
}

fn dispatch_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
) -> DispatchRequest {
    DispatchRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms: 5_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => panic!("expected Issued, got {other:?}"),
    }
}

fn fiber(seed: u8) -> ExecutionFiberId {
    ExecutionFiberId::from_bytes([seed; 16])
}

fn generation(seed: u64) -> Generation {
    Generation::new(std::num::NonZeroU64::new(seed).expect("test generations are non-zero"))
}

fn registration_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    binding: ExecutionFiberId,
    fiber_generation: Generation,
    key_seed: u8,
) -> nlos_task::RegisterEffectBindingRequest {
    nlos_task::RegisterEffectBindingRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        binding,
        fiber_generation,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        registered_at_ms: 4_500,
    }
}

fn registered(decision: EffectBindingDecision) -> EffectFiberRegistrationRecord {
    match decision {
        EffectBindingDecision::Registered(record) => *record,
        EffectBindingDecision::Replayed(_) => panic!("fresh registration cannot replay"),
    }
}

fn replayed(decision: EffectBindingDecision) -> EffectFiberRegistrationRecord {
    match decision {
        EffectBindingDecision::Replayed(record) => *record,
        EffectBindingDecision::Registered(_) => panic!("expected Replayed, got Registered"),
    }
}

/// Registers a task plus two competing attempts, and issues the winner's
/// `CommitPermit` with the given declared effect set.
fn setup_winner(
    effects: Vec<PlannedEffect>,
) -> (TestDatabase, AttemptSpec, AttemptSpec, PermitRecord) {
    let database = TestDatabase::new("setup");
    let authority = database.open();
    authority.register_task(task_spec()).expect("register task");
    let bundle = snapshot(0, 0);
    let spec_a = attempt_spec(0x0a, bundle);
    let spec_b = attempt_spec(0x0b, bundle);
    authority.register_attempt(spec_a).expect("register A");
    authority.register_attempt(spec_b).expect("register B");
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authorities_struct(
                nlos_task::Authorities::default(),
                permit_request(&spec_a, 0x01, effects),
            )
            .expect("permit A"),
    );
    drop(authority);
    (database, spec_a, spec_b, permit)
}

#[test]
fn effect_binding_registration_precedes_effect_and_replays() {
    let (database, spec, _loser, permit) = setup_winner(vec![planned(0, true), planned(1, true)]);
    let authority = database.open();

    let first = registered(
        authority
            .register_effect_binding(registration_request(
                &spec,
                &permit,
                0,
                fiber(0x70),
                generation(1),
                0xd1,
            ))
            .expect("register binding"),
    );
    assert_eq!(first.binding, fiber(0x70));
    assert_eq!(first.fiber_generation, generation(1));
    assert_eq!(first.effect_seq, 0);
    assert_eq!(first.permit_id, permit.permit_id);
    assert_eq!(first.slot_state, SlotState::Planned);

    // The exact replay is byte-equal, under the same key.
    let again = replayed(
        authority
            .register_effect_binding(registration_request(
                &spec,
                &permit,
                0,
                fiber(0x70),
                generation(1),
                0xd1,
            ))
            .expect("replay registration"),
    );
    assert_eq!(again, first);

    // The all-zero value is not a binding: fail closed before any read.
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            1,
            ExecutionFiberId::from_bytes([0; 16]),
            generation(1),
            0xd3,
        )),
        Err(TaskStoreError::InvalidFiberBinding)
    ));

    // The projection read sees the registration joined with its live slot,
    // isolated to the registered binding.
    let listed = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list registrations");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], first);
    assert!(
        authority
            .list_effect_registrations_for_binding(fiber(0x71))
            .expect("list other binding")
            .is_empty()
    );

    // Registration precedes the effect: the slot is still free to issue its
    // effect permit and dispatch after the registration landed.
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("effect permit"),
    );
    assert_eq!(issued.effect_seq, 0);

    // Registration is not required before issuance either: an unregistered
    // slot still issues (the registration is additive evidence, not a gate).
    issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 1, 0xe2))
            .expect("unregistered slot still issues"),
    );
}

#[test]
fn effect_binding_gates_fail_closed_without_side_effects() {
    let (database, spec, loser, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();

    // A non-holder attempt cannot register (`[TASK-RACE-001]` mirror).
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &loser,
            &permit,
            0,
            fiber(0x70),
            generation(1),
            0xd1,
        )),
        Err(TaskStoreError::NotPermitHolder)
    ));
    // A stale permit epoch cannot register.
    let mut stale = registration_request(&spec, &permit, 0, fiber(0x70), generation(1), 0xd2);
    stale.permit_epoch = permit.permit_epoch + 1;
    assert!(matches!(
        authority.register_effect_binding(stale),
        Err(TaskStoreError::PermitEpochMismatch)
    ));

    // Registration and slot binding stay absent after the failed gates.
    assert!(
        authority
            .list_effect_registrations_for_binding(fiber(0x70))
            .expect("list after failures")
            .is_empty()
    );

    // The closed window: once the slot dispatched, registration is too late.
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe3))
            .expect("effect permit"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("dispatch");
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            0,
            fiber(0x70),
            generation(1),
            0xd4,
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Dispatched
        })
    ));
    assert!(
        authority
            .list_effect_registrations_for_binding(fiber(0x70))
            .expect("list after closed window")
            .is_empty()
    );
}

#[test]
fn effect_binding_identity_rebinding_fails_closed() {
    let (database, spec, _loser, permit) = setup_winner(vec![planned(0, true), planned(1, true)]);
    let authority = database.open();

    registered(
        authority
            .register_effect_binding(registration_request(
                &spec,
                &permit,
                0,
                fiber(0x70),
                generation(1),
                0xd1,
            ))
            .expect("register binding"),
    );

    // A different fiber cannot claim the bound slot, even under a fresh key.
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            0,
            fiber(0x71),
            generation(1),
            0xd5,
        )),
        Err(TaskStoreError::EffectBindingConflict)
    ));

    // A stale incarnation of the same binding cannot re-register either.
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            0,
            fiber(0x70),
            generation(2),
            0xd6,
        )),
        Err(TaskStoreError::EffectBindingConflict)
    ));

    // The same key cannot rebind to a different slot.
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            1,
            fiber(0x70),
            generation(1),
            0xd1,
        )),
        Err(TaskStoreError::IdempotencyConflict)
    ));

    // Zero durable side effects from every conflict: exactly one
    // registration exists, bound to slot 0 under generation 1.
    let listed = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list after conflicts");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].effect_seq, 0);
    assert_eq!(listed[0].fiber_generation, generation(1));
}

/// The ADR-0012 write-window matrix for the effect-plane registration: one
/// `Immediate` transaction commits slot columns and registration receipt
/// together, so the durable states are exactly `[absent | complete]` and
/// every redo converges by idempotent replay (ADR-0007/0008 precedent).
#[test]
fn effect_binding_write_window_converges_by_idempotent_replay() {
    let (database, spec, _loser, permit) = setup_winner(vec![planned(0, true)]);
    let expected = {
        let authority = database.open();

        // W1 crash before the write: nothing durable, redo registers fresh.
        assert!(
            authority
                .list_effect_registrations_for_binding(fiber(0x70))
                .expect("absent before any write")
                .is_empty()
        );
        let record = registered(
            authority
                .register_effect_binding(registration_request(
                    &spec,
                    &permit,
                    0,
                    fiber(0x70),
                    generation(1),
                    0xd1,
                ))
                .expect("redo registers fresh"),
        );
        drop(authority);
        record
    };

    // W2 crash after the write commit: the new "process" reopens, the row is
    // wholly present, and the redo with the same key replays byte-equal.
    let authority = database.open();
    let replay = replayed(
        authority
            .register_effect_binding(registration_request(
                &spec,
                &permit,
                0,
                fiber(0x70),
                generation(1),
                0xd1,
            ))
            .expect("redo after reopen replays"),
    );
    assert_eq!(replay, expected);

    // W3 redo with a conflicting identity after commit: fail closed, the
    // durable row stays exactly as committed.
    assert!(matches!(
        authority.register_effect_binding(registration_request(
            &spec,
            &permit,
            0,
            fiber(0x71),
            generation(1),
            0xd7,
        )),
        Err(TaskStoreError::EffectBindingConflict)
    ));
    let listed = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list after window matrix");
    assert_eq!(listed, vec![expected.clone()]);

    // The reopened authority ran the v41 migration idempotently: the schema
    // version stays 41 and the migrated registrations survive re-open again.
    {
        let raw = Connection::open(&database.path).expect("open raw connection");
        assert_eq!(
            raw.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
                .expect("read version"),
            41
        );
    }
    let reopened = database.open();
    assert_eq!(
        reopened
            .list_effect_registrations_for_binding(fiber(0x70))
            .expect("list after second reopen"),
        vec![expected.clone()]
    );
}

#[test]
fn projection_read_tracks_slot_lifecycle_and_terminal_receipt() {
    let (database, spec, _loser, permit) = setup_winner(vec![planned(0, true)]);
    let authority = database.open();

    registered(
        authority
            .register_effect_binding(registration_request(
                &spec,
                &permit,
                0,
                fiber(0x70),
                generation(1),
                0xd1,
            ))
            .expect("register binding"),
    );
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("effect permit"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("dispatch");

    // Mid-flight: the projection read shows the dispatched slot.
    let mid_flight = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list mid-flight");
    assert_eq!(mid_flight[0].slot_state, SlotState::Dispatched);
    assert!(mid_flight[0].effect_receipt_id.is_none());

    // The effect closes: the joined read carries the terminal state and the
    // receipt id (the effect-completion fact of the replayed stream).
    let receipt = match authority
        .record_effect_outcome(OutcomeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0x99; 32],
            },
            recorded_at_ms: 6_000,
        })
        .expect("record outcome")
    {
        EffectReceiptDecision::Recorded(receipt) => receipt,
        EffectReceiptDecision::Replayed(_) => panic!("expected Recorded, got Replayed"),
    };
    let closed = authority
        .list_effect_registrations_for_binding(fiber(0x70))
        .expect("list closed");
    assert_eq!(closed[0].slot_state, SlotState::EffectClosed);
    assert_eq!(closed[0].effect_receipt_id, Some(receipt.receipt_id));

    // The registration row is immutable and the permit still finalizes.
    assert!(matches!(
        authority.finalize_commit(FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            new_effect_history_root: empty_effect_set_root(),
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        }),
        Ok(FinalizeDecision::Committed(_))
    ));
}

#[allow(dead_code)]
fn unused_helpers() {
    // Keep the idempotency-identity helper linked for parity with sibling
    // effect test files.
    let _ = idempotency_identity_digest;
    let _ = CommitPermitId::from_bytes(bytes(0));
}
