//! TK-B2 acceptance tests: the authority-lease fence extends from the
//! terminal paths (finalize/close/adopt/reconcile) to the five effect-plane
//! write entries — `EffectPermit` issuance, dispatch-token consumption,
//! outcome registration, no-effect closure, and effect-fiber registration.
//!
//! Boundary semantics under test (`validate_permit_authority_lease`, the
//! exact helper the terminal paths use):
//!
//! * a permit issued WITHOUT a lease binding (the issuance opt-in default)
//!   keeps the legacy unfenced behavior on every entry — the fence never
//!   widens to unbound permits;
//! * a lease-bound permit must present its exact live lease on every entry:
//!   no presentation fails with `AuthorityLeaseRequired`, a foreign lease
//!   with `AuthorityLeaseBindingMismatch`, and the previously-bound (but
//!   no longer live) lease with `AuthorityLeaseFenced`;
//! * after `acquire_authority_lease` hands the lease to a new holder, the
//!   old holder's five write attempts all fail closed with zero partial
//!   state;
//! * exact idempotency replays stay FIRST: byte-equal re-presentations
//!   return the original durable decision without consulting the lease.

#![allow(deprecated)] // The minimal fixture copies the legacy commit-permit ladder entry.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, AuthorityLeaseDecision, AuthorityLeaseDispatchRequest,
    AuthorityLeaseEffectBindingRequest, AuthorityLeaseEffectPermitRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    EffectBindingDecision, EffectPermitDecision, EffectPermitRequest, EffectReceiptDecision,
    IssuedPermit, LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome,
    OutcomeRequest, PermitDecision, PermitRecord, PlannedEffect, RegisterEffectBindingRequest,
    SlotRecord, SlotState, SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError,
};
use nlos_types::{
    CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, ProcessId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-effect-lease-fence-{name}-{}-{sequence}.sqlite3",
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

fn snapshot() -> SnapshotBundle {
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10)),
        snapshot_digest: [0x20; 32],
        expected_head_commit_seq: 0,
        effect_history_root: nlos_task::empty_effect_history_root(),
        retry_fence_epoch: 0,
    }
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(0x0a)),
        attempt_generation: Generation::INITIAL,
        snapshot: snapshot(),
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xca)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xaa)),
        registered_at_ms: 2_000,
    }
}

fn planned(stable_action_slot: u64) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x44; 32],
            stable_action_slot,
            target_authority_object_id: [0x55; 32],
            effect_class: 7,
            idempotency_scope: 3,
        },
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn commit_request(effects: Vec<PlannedEffect>) -> nlos_task::PermitRequest {
    let spec = attempt_spec();
    nlos_task::PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [0x01; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb1)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn lease_request(
    holder_seed: u8,
    key_seed: u8,
    requested_at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes(bytes(holder_seed)),
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        requested_at_ms,
        ttl_ms,
    }
}

fn lease_record(decision: AuthorityLeaseDecision) -> AuthorityLeaseRecord {
    decision.record()
}

fn effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
    requested_at_ms: i64,
) -> EffectPermitRequest {
    EffectPermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms: 9_000,
        requested_at_ms,
    }
}

fn dispatch_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
    dispatched_at_ms: i64,
) -> nlos_task::DispatchRequest {
    nlos_task::DispatchRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms,
    }
}

fn outcome_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    recorded_at_ms: i64,
) -> OutcomeRequest {
    OutcomeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        outcome: Outcome::Closed {
            authoritative_closure_digest: [0xaa; 32],
        },
        recorded_at_ms,
    }
}

fn no_effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    token: Option<[u8; 32]>,
    recorded_at_ms: i64,
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason: NoEffectReason::NotSelected,
        dispatch_token: token,
        recorded_at_ms,
    }
}

fn binding_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    fiber_seed: u8,
    key_seed: u8,
    registered_at_ms: i64,
) -> RegisterEffectBindingRequest {
    RegisterEffectBindingRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        binding: ExecutionFiberId::from_bytes(bytes(fiber_seed)),
        fiber_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        registered_at_ms,
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

/// Re-reads an already-minted permit by replaying its exact request; the
/// durable decision (fresh or replayed) carries the same one-shot token.
fn reread_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) | EffectPermitDecision::Replayed(record) => *record,
    }
}

fn registered_binding(decision: EffectBindingDecision) {
    match decision {
        EffectBindingDecision::Registered(_) => {}
        other @ EffectBindingDecision::Replayed(_) => panic!("expected Registered, got {other:?}"),
    }
}

fn recorded_receipt(decision: EffectReceiptDecision) {
    match decision {
        EffectReceiptDecision::Recorded(_) => {}
        other @ EffectReceiptDecision::Replayed(_) => panic!("expected Recorded, got {other:?}"),
    }
}

/// The TK-B2 scenario prefix. Holder one acquires the authority lease at
/// `2_500` (TTL `2_000` → expires `4_500`), issues a five-slot lease-bound
/// `CommitPermit` at `3_000`, then drives the effect plane through the
/// lease-presenting rungs — every step succeeds, which is the
/// "correct lease presented → allowed" control:
///
/// * slot 0: fiber registration at `3_050` (stays `Planned`);
/// * slots 1 and 2: `EffectPermit` mints at `3_100`/`3_110` (stay
///   `Permitted`);
/// * slot 3: mint at `3_120` then dispatch at `3_400` (ends `Dispatched`);
/// * slot 4: untouched (`Planned`), the fresh-write target after takeover.
fn bound_fixture() -> (
    TestDatabase,
    AttemptSpec,
    PermitRecord,
    AuthorityLeaseRecord,
) {
    let database = TestDatabase::new("bound");
    let authority = database.open();
    let spec = attempt_spec();
    authority.register_task(task_spec()).expect("register task");
    authority.register_attempt(spec).expect("register attempt");
    let lease_one = lease_record(
        authority
            .acquire_authority_lease(lease_request(0x81, 0x82, 2_500, 2_000))
            .expect("holder one lease"),
    );
    let effects: Vec<PlannedEffect> = (0..5).map(planned).collect();
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: commit_request(effects),
                lease: lease_one,
            })
            .expect("lease-bound commit permit"),
    );
    assert_eq!(permit.authority_lease_binding, Some(lease_one.binding()));

    // Correct-lease control: the fence is open to the live lease holder.
    registered_binding(
        authority
            .register_effect_binding_with_authority_lease(AuthorityLeaseEffectBindingRequest {
                binding: binding_request(&spec, &permit, 0, 0xf1, 0xf3, 3_050),
                lease: lease_one,
            })
            .expect("register slot 0 behind the live lease"),
    );
    let mint = |effect_seq: u64, key_seed: u8, at_ms: i64| {
        issued_effect_permit(
            authority
                .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
                    permit: effect_request(&spec, &permit, effect_seq, key_seed, at_ms),
                    lease: lease_one,
                })
                .expect("mint behind the live lease"),
        )
    };
    let _slot_one = mint(1, 0xe1, 3_100);
    let _slot_two = mint(2, 0xe2, 3_110);
    let slot_three = mint(3, 0xe3, 3_120);
    authority
        .consume_dispatch_token_with_authority_lease(AuthorityLeaseDispatchRequest {
            dispatch: dispatch_request(&spec, &permit, &slot_three, 3_400),
            lease: lease_one,
        })
        .expect("dispatch behind the live lease");
    drop(authority);
    (database, spec, permit, lease_one)
}

/// Hands the lease to a second holder at `5_000` (the first lease expired
/// at `4_500`) and proves the takeover actually advanced term and epoch.
fn take_over(database: &TestDatabase) -> AuthorityLeaseRecord {
    let authority = database.open();
    match authority
        .acquire_authority_lease(lease_request(0x91, 0x92, 5_000, 5_000))
        .expect("holder two takeover")
    {
        AuthorityLeaseDecision::TakenOver(record) => {
            assert_eq!(record.term, 2, "the expired lease advances the term");
            assert_eq!(record.lease_epoch, 2, "the takeover advances the epoch");
            record
        }
        other => panic!("expected TakenOver, got {other:?}"),
    }
}

fn slot(authority: &SqliteTaskAuthority, permit: &PermitRecord, effect_seq: u64) -> SlotRecord {
    authority
        .inspect_effect_slot(permit.permit_id, effect_seq)
        .expect("slot")
}

/// Bullet ①: the fence is live from issuance (a bound permit without its
/// lease is refused even BEFORE any takeover, exactly like finalize), and
/// after the lease changes hands the stale holder's five write attempts all
/// fail closed with `AuthorityLeaseRequired`, leaving zero partial state.
#[test]
fn takeover_fences_every_effect_plane_write_entry() {
    let (database, spec, permit, lease_one) = bound_fixture();
    let authority = database.open();

    // Pre-takeover: the bound permit already requires its lease — the plain
    // (lease-absent) mint is refused fail-closed and moves nothing.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec, &permit, 4, 0xe4, 3_500)),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert_eq!(slot(&authority, &permit, 4).state, SlotState::Planned);

    let _lease_two = take_over(&database);

    // The five entries, driven by the stale holder without a lease. Every
    // observation time is strictly inside the `EffectPermit` TTL (9_000),
    // so the rejections below are the lease fence, not expiry.
    assert!(matches!(
        authority.request_effect_permit(effect_request(&spec, &permit, 4, 0xe4, 5_500)),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    let slot_two = reread_effect_permit(
        authority
            .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
                permit: effect_request(&spec, &permit, 2, 0xe2, 3_110),
                lease: lease_one,
            })
            .expect("re-read slot 2 permit for its token"),
    );
    assert!(matches!(
        authority.consume_dispatch_token(dispatch_request(&spec, &permit, &slot_two, 5_500)),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(&spec, &permit, 3, 5_500)),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    let slot_one = reread_effect_permit(
        authority
            .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
                permit: effect_request(&spec, &permit, 1, 0xe1, 3_100),
                lease: lease_one,
            })
            .expect("re-read slot 1 permit for its token"),
    );
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            Some(slot_one.one_shot_dispatch_token),
            5_500
        )),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert!(matches!(
        authority.register_effect_binding(binding_request(&spec, &permit, 4, 0xf4, 0xf5, 5_500)),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));

    // Zero partial state: every slot keeps its durable state and sequence,
    // and the task's control epoch never advanced across the five refusals.
    for (effect_seq, state, state_seq) in [
        (0u64, SlotState::Planned, 0),
        (1, SlotState::Permitted, 1),
        (2, SlotState::Permitted, 1),
        (3, SlotState::Dispatched, 2),
        (4, SlotState::Planned, 0),
    ] {
        let record = slot(&authority, &permit, effect_seq);
        assert_eq!(record.state, state, "slot {effect_seq} state");
        assert_eq!(record.state_seq, state_seq, "slot {effect_seq} sequence");
    }
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        6,
        "task starts at 1; commit-permit+mint×3+dispatch committed 5 control epochs; the refusals added none"
    );

    // Replaying the exact refused mint re-derives the identical rejection.
    let first = authority
        .request_effect_permit(effect_request(&spec, &permit, 4, 0xe4, 5_500))
        .expect_err("replay re-derives the refusal");
    assert!(matches!(first, TaskStoreError::AuthorityLeaseRequired));
}

/// Bullet ②: presenting the stale (previously bound, no longer live) lease
/// fails closed with `AuthorityLeaseFenced`, and presenting the successor's
/// foreign lease fails closed with `AuthorityLeaseBindingMismatch`.
#[test]
fn stale_and_foreign_lease_presentations_fail_closed_after_takeover() {
    let (database, spec, permit, lease_one) = bound_fixture();
    let lease_two = take_over(&database);
    let authority = database.open();

    assert!(matches!(
        authority.request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
            permit: effect_request(&spec, &permit, 4, 0xe4, 5_500),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    let slot_two = reread_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 2, 0xe2, 3_110))
            .expect("replayed mint without consulting the lease"),
    );
    assert!(matches!(
        authority.consume_dispatch_token_with_authority_lease(AuthorityLeaseDispatchRequest {
            dispatch: dispatch_request(&spec, &permit, &slot_two, 5_500),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));

    // The successor's live lease is foreign to the permit's bound lease.
    assert!(matches!(
        authority.request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
            permit: effect_request(&spec, &permit, 4, 0xe4, 5_500),
            lease: lease_two,
        }),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));
}

/// Bullet ③ (control): a permit issued WITHOUT a lease binding keeps the
/// legacy unfenced behavior — the same five plain entries all succeed and
/// the opt-in fence never widens to unbound permits.
#[test]
fn unbound_permit_keeps_unfenced_effect_plane() {
    let database = TestDatabase::new("unbound");
    let authority = database.open();
    let spec = attempt_spec();
    authority.register_task(task_spec()).expect("register task");
    authority.register_attempt(spec).expect("register attempt");
    let effects: Vec<PlannedEffect> = (0..5).map(planned).collect();
    let permit = issued_permit(
        authority
            .request_commit_permit(commit_request(effects))
            .expect("unbound commit permit"),
    );
    assert_eq!(permit.authority_lease_binding, None);

    registered_binding(
        authority
            .register_effect_binding(binding_request(&spec, &permit, 0, 0xf1, 0xf3, 3_050))
            .expect("plain registration stays unfenced"),
    );
    for (effect_seq, key_seed) in [(1u64, 0xe1u8), (2, 0xe2), (3, 0xe3)] {
        issued_effect_permit(
            authority
                .request_effect_permit(effect_request(&spec, &permit, effect_seq, key_seed, 3_100))
                .expect("plain mint stays unfenced"),
        );
    }
    let slot_one = reread_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 1, 0xe1, 3_100))
            .expect("mint replay for the token"),
    );
    recorded_receipt(
        authority
            .record_no_effect(no_effect_request(
                &spec,
                &permit,
                1,
                Some(slot_one.one_shot_dispatch_token),
                4_000,
            ))
            .expect("plain no-effect stays unfenced"),
    );
    let slot_two = reread_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 2, 0xe2, 3_100))
            .expect("mint replay for the token"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &slot_two, 3_400))
        .expect("plain dispatch stays unfenced");
    recorded_receipt(
        authority
            .record_effect_outcome(outcome_request(&spec, &permit, 2, 4_500))
            .expect("plain outcome stays unfenced"),
    );

    let no_effect = slot(&authority, &permit, 1);
    assert_eq!(no_effect.state, SlotState::NoEffect);
    let closed = slot(&authority, &permit, 2);
    assert_eq!(closed.state, SlotState::EffectClosed);
}

/// Bullet ④: exact idempotency replays stay AHEAD of the lease fence —
/// after the takeover, byte-equal re-presentations return the original
/// durable decisions (issuance via the plain entry, fiber registration via
/// the lease rung with the now-stale lease) without consulting the lease.
#[test]
fn replay_branches_stay_ahead_of_the_lease_fence() {
    let (database, spec, permit, lease_one) = bound_fixture();
    let _lease_two = take_over(&database);
    let authority = database.open();

    match authority
        .request_effect_permit(effect_request(&spec, &permit, 1, 0xe1, 3_100))
        .expect("mint replay must not consult the lease")
    {
        EffectPermitDecision::Replayed(record) => {
            assert_eq!(record.effect_seq, 1);
            assert_eq!(record.valid_until_ms, 9_000);
        }
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    }
    match authority
        .register_effect_binding_with_authority_lease(AuthorityLeaseEffectBindingRequest {
            binding: binding_request(&spec, &permit, 0, 0xf1, 0xf3, 3_050),
            lease: lease_one,
        })
        .expect("registration replay must not consult the lease")
    {
        EffectBindingDecision::Replayed(record) => {
            assert_eq!(record.effect_seq, 0);
        }
        other @ EffectBindingDecision::Registered(_) => panic!("expected Replayed, got {other:?}"),
    }
}

/// Bullet ⑤: the expired-but-current lease. No takeover has happened, so
/// the permit's bound lease is still byte-for-byte the durable one — it is
/// simply past its own `expires_at_ms` (`4_500`). Presenting it at the
/// effect plane fails closed with the `AuthorityLeaseExpired` family: not
/// `AuthorityLeaseRequired` (a lease WAS presented) and not
/// `AuthorityLeaseFenced` (nothing has superseded the lease bytes).
#[test]
fn expired_not_taken_over_lease_is_rejected_as_expired_not_fenced() {
    let (database, spec, permit, lease_one) = bound_fixture();
    let authority = database.open();

    // Scenario control: the bound lease is still the durable one, and its
    // rejection at the lease validator is expiry, not fencing.
    assert!(matches!(
        authority.validate_authority_lease(lease_one, 5_000),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));

    // Mint (slot 4, fresh write target): the expired lease is presented
    // and refused with the expiry family.
    assert!(matches!(
        authority.request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
            permit: effect_request(&spec, &permit, 4, 0xe4, 5_500),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));
    assert_eq!(slot(&authority, &permit, 4).state, SlotState::Planned);

    // Dispatch (slot 2, token re-read via the exact mint replay): the
    // same expired lease is refused with the same family.
    let slot_two = reread_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 2, 0xe2, 3_110))
            .expect("replayed mint without consulting the lease"),
    );
    assert!(matches!(
        authority.consume_dispatch_token_with_authority_lease(AuthorityLeaseDispatchRequest {
            dispatch: dispatch_request(&spec, &permit, &slot_two, 5_500),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseExpired)
    ));

    // Zero partial state: the observed slot keeps its durable state and
    // sequence, and the two expiry refusals added no control epoch.
    let slot_four = slot(&authority, &permit, 4);
    assert_eq!(slot_four.state, SlotState::Planned);
    assert_eq!(slot_four.state_seq, 0);
    let slot_two_record = slot(&authority, &permit, 2);
    assert_eq!(slot_two_record.state, SlotState::Permitted);
    assert_eq!(slot_two_record.state_seq, 1);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        6,
        "the refusals added no control epoch"
    );
}
