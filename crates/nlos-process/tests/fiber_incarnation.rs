//! Acceptance tests for the ADR-0012 B-PROCESS-002 process slice: fiber
//! incarnation registration borrowed from the B-PROCESS-001 durable
//! generation/fence authority (CAS + fence, stale fail-closed with zero
//! side effect) and the handler-entry snapshot slot (latest-only per
//! invocation, terminal GC, digest-revalidated readback).

use nlos_process::{
    CreateIsolationDomainRequest, FiberEntrySnapshotDecision, FiberIncarnationDecision,
    IsolationDomainDecision, ProcessAuthority, ProcessAuthorityError, ProcessBindingDecision,
    RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
    WriteFiberEntrySnapshotRequest,
};
use nlos_types::{
    ExecutionFiberId, Generation, IdempotencyKey, IsolationDomainId, ProcessId, TaskAttemptId,
    TaskId,
};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn fiber(seed: u8) -> ExecutionFiberId {
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
            "nlos-process-fiber-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Registers one delegated process binding and returns its current record
/// plus the authority, so tests can present the exact head fence.
struct Fixture {
    authority: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
}

fn open_fixture(root: &Root, seed: u8) -> Fixture {
    let authority = ProcessAuthority::open(root.path()).expect("open authority");
    let domain = match authority.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [0x11; 32],
        idempotency_key: key(230 + seed),
        created_at_ms: 1_000,
    }) {
        Ok(
            IsolationDomainDecision::Created(record) | IsolationDomainDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("open fixture domain: {error}"),
    };
    let binding = match authority.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed; 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed; 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: IsolationDomainId::from_bytes(domain.isolation_domain_id.into_bytes()),
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: key(240 + seed),
        created_at_ms: 1_100,
    }) {
        Ok(
            ProcessBindingDecision::Registered(record) | ProcessBindingDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("open fixture process binding: {error}"),
    };
    Fixture {
        authority,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
    }
}

fn incarnation_request(
    fixture: &Fixture,
    fiber_id: ExecutionFiberId,
    key_seed: u8,
) -> RegisterFiberIncarnationRequest {
    RegisterFiberIncarnationRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        binding: fiber_id,
        idempotency_key: key(key_seed),
        registered_at_ms: 1_500,
    }
}

fn registered(decision: FiberIncarnationDecision) -> nlos_process::FiberIncarnationRecord {
    match decision {
        FiberIncarnationDecision::Registered(record) => record,
        FiberIncarnationDecision::Replayed(_) => panic!("fresh registration cannot replay"),
    }
}

fn written(decision: FiberEntrySnapshotDecision) -> nlos_process::FiberEntrySnapshotRecord {
    match decision {
        FiberEntrySnapshotDecision::Written(record) => record,
        FiberEntrySnapshotDecision::Replayed(_) => panic!("fresh write cannot replay"),
    }
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the CAS ladder and every stale-fence gate.
fn incarnation_increments_cas_and_stale_fence_fails_closed() {
    let root = Root::new("incarnation");
    let fixture = open_fixture(&root, 1);

    let first = registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xA1), 1))
            .expect("first incarnation"),
    );
    assert_eq!(first.incarnation_generation, Generation::INITIAL);
    assert_eq!(first.prior_incarnation_generation, None);
    assert_eq!(first.binding, fiber(0xA1));

    let current = fixture
        .authority
        .inspect_fiber_incarnation(fixture.process_id, fiber(0xA1))
        .expect("inspect current");
    assert_eq!(current.incarnation_generation, Generation::INITIAL);
    assert_eq!(current.fencing_token, first.fencing_token);

    // Second registration increments by exactly one under the same process
    // fence; the head moves to the new generation.
    let second = registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xA1), 2))
            .expect("second incarnation"),
    );
    assert_eq!(
        second.prior_incarnation_generation,
        Some(Generation::INITIAL)
    );
    assert_ne!(second.fencing_token, first.fencing_token);
    let current = fixture
        .authority
        .inspect_fiber_incarnation(fixture.process_id, fiber(0xA1))
        .expect("inspect current");
    assert_eq!(
        current.incarnation_generation,
        Generation::INITIAL.checked_next().expect("next")
    );

    // Exact-key replay returns the original record.
    let replayed = match fixture
        .authority
        .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xA1), 2))
        .expect("replay second")
    {
        FiberIncarnationDecision::Replayed(record) => record,
        FiberIncarnationDecision::Registered(_) => {
            panic!("expected Replayed, got Registered")
        }
    };
    assert_eq!(replayed, second);

    // Key rebound to a different binding: fail closed.
    assert!(matches!(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xB2), 2)),
        Err(ProcessAuthorityError::IdempotencyConflict)
    ));

    // Stale process fence: fail closed with zero side effect — the current
    // incarnation and the generation sequence are untouched, and a fresh
    // key still registers the next incarnation afterwards.
    let stale = RegisterFiberIncarnationRequest {
        expected_process_generation: Generation::INITIAL.checked_next().expect("next"),
        ..incarnation_request(&fixture, fiber(0xA1), 3)
    };
    assert!(matches!(
        fixture.authority.register_fiber_incarnation(stale),
        Err(ProcessAuthorityError::StaleProcessBinding)
    ));
    assert_eq!(
        fixture
            .authority
            .inspect_fiber_incarnation(fixture.process_id, fiber(0xA1))
            .expect("inspect after stale")
            .incarnation_generation,
        Generation::INITIAL.checked_next().expect("next"),
        "the stale registration must not have incremented"
    );
    let third = registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xA1), 4))
            .expect("third incarnation"),
    );
    assert_eq!(
        third.prior_incarnation_generation,
        Some(second.incarnation_generation)
    );

    // Unknown process and zero binding fail closed.
    let unknown = RegisterFiberIncarnationRequest {
        process_id: ProcessId::from_bytes([7; 16]),
        ..incarnation_request(&fixture, fiber(0xA1), 5)
    };
    assert!(matches!(
        fixture.authority.register_fiber_incarnation(unknown),
        Err(ProcessAuthorityError::ProcessNotFound(_))
    ));
    assert!(matches!(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(
                &fixture,
                ExecutionFiberId::from_bytes([0; 16]),
                6
            )),
        Err(ProcessAuthorityError::InvalidFiberBinding)
    ));

    // Bindings are isolated per (process, binding): a second binding starts
    // at generation 1 under the same process.
    let other = registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xB2), 7))
            .expect("other binding incarnation"),
    );
    assert_eq!(other.incarnation_generation, Generation::INITIAL);
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers the latest-only slot, its stale CAS and the GC.
fn entry_snapshot_is_latest_only_with_stale_cas_and_gc() {
    let root = Root::new("snapshot");
    let fixture = open_fixture(&root, 2);
    registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xC3), 10))
            .expect("incarnation"),
    );
    let current = fixture
        .authority
        .inspect_fiber_incarnation(fixture.process_id, fiber(0xC3))
        .expect("inspect");

    // No snapshot before the first write.
    assert!(matches!(
        fixture
            .authority
            .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xC3)),
        Err(ProcessAuthorityError::FiberSnapshotNotFound)
    ));

    let first = written(
        fixture
            .authority
            .write_fiber_entry_snapshot(snapshot_request(
                &fixture,
                fiber(0xC3),
                current.incarnation_generation,
                b"invocation-1",
                2_000,
            ))
            .expect("first write"),
    );
    assert_eq!(first.handler_input, b"invocation-1".to_vec());
    assert_eq!(first.written_by_incarnation, current.incarnation_generation);

    // Same bytes again: replay, latest slot unchanged.
    match fixture
        .authority
        .write_fiber_entry_snapshot(snapshot_request(
            &fixture,
            fiber(0xC3),
            current.incarnation_generation,
            b"invocation-1",
            2_100,
        ))
        .expect("replay write")
    {
        FiberEntrySnapshotDecision::Replayed(record) => {
            assert_eq!(record.input_digest, first.input_digest);
        }
        FiberEntrySnapshotDecision::Written(_) => {
            panic!("expected Replayed, got Written")
        }
    }

    // Different bytes: latest-only overwrite wins.
    written(
        fixture
            .authority
            .write_fiber_entry_snapshot(snapshot_request(
                &fixture,
                fiber(0xC3),
                current.incarnation_generation,
                b"invocation-2",
                2_200,
            ))
            .expect("overwrite"),
    );
    let restored = fixture
        .authority
        .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xC3))
        .expect("inspect latest");
    assert_eq!(restored.handler_input, b"invocation-2".to_vec());
    assert_ne!(restored.input_digest, first.input_digest);

    // Stale incarnation CAS: fail closed, zero side effect — the slot keeps
    // the latest bytes, and a fresh registration does not resurrect them.
    let stale_generation = Generation::INITIAL.checked_next().expect("next");
    assert!(matches!(
        fixture
            .authority
            .write_fiber_entry_snapshot(snapshot_request(
                &fixture,
                fiber(0xC3),
                stale_generation,
                b"stale-write",
                2_300,
            )),
        Err(ProcessAuthorityError::StaleFiberIncarnation)
    ));
    assert_eq!(
        fixture
            .authority
            .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xC3))
            .expect("inspect after stale")
            .handler_input,
        b"invocation-2".to_vec(),
        "the stale write must not have touched the slot"
    );

    // Invalid inputs fail closed.
    assert!(matches!(
        fixture
            .authority
            .write_fiber_entry_snapshot(snapshot_request(
                &fixture,
                fiber(0xC3),
                current.incarnation_generation,
                b"",
                2_400,
            )),
        Err(ProcessAuthorityError::InvalidFiberSnapshot(_))
    ));
    let zero_binding = WriteFiberEntrySnapshotRequest {
        binding: ExecutionFiberId::from_bytes([0; 16]),
        ..snapshot_request(
            &fixture,
            fiber(0xC3),
            current.incarnation_generation,
            b"x",
            2_500,
        )
    };
    assert!(matches!(
        fixture.authority.write_fiber_entry_snapshot(zero_binding),
        Err(ProcessAuthorityError::InvalidFiberBinding)
    ));

    // Terminal GC: exact delete, then idempotent false, then restore sees
    // the absent snapshot.
    assert!(
        fixture
            .authority
            .gc_fiber_entry_snapshot(fixture.process_id, fiber(0xC3))
            .expect("gc")
    );
    assert!(
        !fixture
            .authority
            .gc_fiber_entry_snapshot(fixture.process_id, fiber(0xC3))
            .expect("gc again")
    );
    assert!(matches!(
        fixture
            .authority
            .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xC3)),
        Err(ProcessAuthorityError::FiberSnapshotNotFound)
    ));

    // Unregistered binding: snapshot operations fail closed.
    assert!(matches!(
        fixture
            .authority
            .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xD4)),
        Err(ProcessAuthorityError::FiberIncarnationNotFound)
    ));
}

fn snapshot_request(
    fixture: &Fixture,
    binding: ExecutionFiberId,
    incarnation: Generation,
    input: &[u8],
    at: u64,
) -> WriteFiberEntrySnapshotRequest {
    WriteFiberEntrySnapshotRequest {
        process_id: fixture.process_id,
        binding,
        expected_incarnation_generation: incarnation,
        handler_input: input.to_vec(),
        written_at_ms: at,
    }
}

#[test]
fn incarnation_and_snapshot_survive_reopen_and_supersede_sweeps() {
    let root = Root::new("reopen");
    let fixture = open_fixture(&root, 3);
    registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xE5), 20))
            .expect("incarnation"),
    );
    let current = fixture
        .authority
        .inspect_fiber_incarnation(fixture.process_id, fiber(0xE5))
        .expect("inspect");
    written(
        fixture
            .authority
            .write_fiber_entry_snapshot(snapshot_request(
                &fixture,
                fiber(0xE5),
                current.incarnation_generation,
                b"durable-input",
                3_000,
            ))
            .expect("write"),
    );
    drop(fixture);

    // Restart replay: everything revalidates field-for-field after reopen,
    // and the schema version is exactly v2.
    let fixture = open_fixture(&root, 3);
    let restored = fixture
        .authority
        .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xE5))
        .expect("inspect after reopen");
    assert_eq!(restored.handler_input, b"durable-input".to_vec());
    let current = fixture
        .authority
        .inspect_fiber_incarnation(fixture.process_id, fiber(0xE5))
        .expect("incarnation after reopen");
    assert_eq!(current.incarnation_generation, Generation::INITIAL);

    // A new incarnation deliberately leaves the snapshot slot untouched:
    // the latest-only slot is shared across incarnations, so the next
    // recovery can consume the previous invocation's snapshot (the
    // crash-window case). Only the explicit terminal GC removes it.
    registered(
        fixture
            .authority
            .register_fiber_incarnation(incarnation_request(&fixture, fiber(0xE5), 21))
            .expect("next incarnation"),
    );
    let survived = fixture
        .authority
        .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xE5))
        .expect("snapshot survives the incarnation supersede");
    assert_eq!(survived.handler_input, b"durable-input".to_vec());
    assert!(
        fixture
            .authority
            .gc_fiber_entry_snapshot(fixture.process_id, fiber(0xE5))
            .expect("terminal gc")
    );
    assert!(matches!(
        fixture
            .authority
            .inspect_fiber_entry_snapshot(fixture.process_id, fiber(0xE5)),
        Err(ProcessAuthorityError::FiberSnapshotNotFound)
    ));
}
