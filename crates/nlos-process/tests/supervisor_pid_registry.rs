//! Supervisor pid registry minimum prefix (W22-P): `ProcessId` → OS pid
//! managed mapping with generation fencing, plus the adapter feed shape.

use std::collections::HashMap;

use nlos_process::{
    PosixPlatformKillAdapter, RegisterSupervisorPidRequest, SupervisorPidDecision,
    SupervisorPidRegistry, SupervisorPidRegistryError,
};
use nlos_types::{Generation, ProcessId};

fn process_id(seed: u8) -> ProcessId {
    ProcessId::from_bytes([seed; 16])
}

fn register_request(
    process: ProcessId,
    generation: Generation,
    os_pid: u32,
) -> RegisterSupervisorPidRequest {
    RegisterSupervisorPidRequest {
        process_id: process,
        process_generation: generation,
        os_pid,
        registered_at_ms: 1_000,
    }
}

#[test]
fn register_replays_same_generation_and_same_pid() {
    // Given: a fresh registry with one registered mapping.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x01);
    let first = registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("first registration");
    assert!(matches!(first, SupervisorPidDecision::Registered(_)));

    // When: the exact same (process, generation, os pid) is re-presented.
    let replay = registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("replay must succeed");

    // Then: the replay returns the original entry unchanged.
    assert!(matches!(replay, SupervisorPidDecision::Replayed(_)));
    assert_eq!(*replay.current(), *first.current());
    assert_eq!(first.current().registered_at_ms, 1_000);
    assert_eq!(replay.current().registered_at_ms, 1_000);
    assert_eq!(registry.pid_map(), HashMap::from([(process, 4242)]));
}

#[test]
fn same_generation_pid_rebind_is_fail_closed() {
    // Given: a registered mapping for generation INITIAL.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x02);
    registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("initial registration");

    // When: the same generation presents a different OS pid.
    let error = registry
        .register(register_request(process, Generation::INITIAL, 5151))
        .expect_err("same-generation pid rebind must fail closed");

    // Then: the rejection is typed and has zero side effect.
    assert_eq!(
        error,
        SupervisorPidRegistryError::OsPidRebind {
            process_id: process,
            generation: Generation::INITIAL,
            registered: 4242,
            presented: 5151,
        }
    );
    assert_eq!(registry.pid_map(), HashMap::from([(process, 4242)]));
}

#[test]
fn strictly_newer_generation_supersedes_and_returns_previous() {
    // Given: a registered mapping at the initial generation.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x03);
    registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("initial registration");
    let restored = Generation::INITIAL.checked_next().expect("next generation");

    // When: the strictly newer generation registers a new OS pid.
    let decision = registry
        .register(register_request(process, restored, 5151))
        .expect("supersede must succeed");

    // Then: the decision is explicit and carries previous + current.
    let SupervisorPidDecision::Superseded { previous, current } = &decision else {
        panic!("expected Superseded, got {decision:?}");
    };
    assert_eq!(previous.process_generation, Generation::INITIAL);
    assert_eq!(previous.os_pid, 4242);
    assert_eq!(current.process_generation, restored);
    assert_eq!(current.os_pid, 5151);
    assert_eq!(registry.pid_map(), HashMap::from([(process, 5151)]));
}

#[test]
fn older_generation_registration_is_fail_closed() {
    // Given: a mapping already superseded to generation two.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x04);
    let generation_two = Generation::INITIAL.checked_next().expect("next generation");
    registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("initial registration");
    registry
        .register(register_request(process, generation_two, 5151))
        .expect("supersede");

    // When: a stale generation-one registration replays late.
    let error = registry
        .register(register_request(process, Generation::INITIAL, 9999))
        .expect_err("stale generation must fail closed");

    // Then: the rejection is typed and the current mapping is untouched.
    assert_eq!(
        error,
        SupervisorPidRegistryError::StaleProcessGeneration {
            process_id: process,
            current: generation_two,
            presented: Generation::INITIAL,
        }
    );
    assert_eq!(registry.pid_map(), HashMap::from([(process, 5151)]));
}

#[test]
fn unregister_is_idempotent() {
    // Given: one registered mapping.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x05);
    registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("registration");

    // When: the mapping is unregistered twice at the same generation.
    let first = registry
        .unregister(process, Generation::INITIAL)
        .expect("first unregister");
    let second = registry
        .unregister(process, Generation::INITIAL)
        .expect("second unregister");

    // Then: only the first removes a row; the second is the idempotent false.
    assert!(first);
    assert!(!second);
    assert!(registry.pid_map().is_empty());
    assert_eq!(
        registry.lookup(process).expect_err("lookup must miss"),
        SupervisorPidRegistryError::ProcessNotRegistered(process)
    );
}

#[test]
fn unregister_with_stale_generation_is_fail_closed() {
    // Given: a mapping superseded to generation two.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x06);
    let generation_two = Generation::INITIAL.checked_next().expect("next generation");
    registry
        .register(register_request(process, Generation::INITIAL, 4242))
        .expect("initial registration");
    registry
        .register(register_request(process, generation_two, 5151))
        .expect("supersede");

    // When: the stale generation-one unregister arrives.
    let error = registry
        .unregister(process, Generation::INITIAL)
        .expect_err("stale unregister must fail closed");

    // Then: the rejection is typed and the current mapping survives.
    assert_eq!(
        error,
        SupervisorPidRegistryError::StaleProcessGeneration {
            process_id: process,
            current: generation_two,
            presented: Generation::INITIAL,
        }
    );
    assert_eq!(registry.pid_map(), HashMap::from([(process, 5151)]));
}

#[test]
fn lookup_miss_is_typed() {
    // Given: a fresh registry.
    let registry = SupervisorPidRegistry::new();
    let process = process_id(0x07);

    // When: an unknown ProcessId is looked up.
    let error = registry.lookup(process).expect_err("lookup must miss");

    // Then: the miss carries the requested identity.
    assert_eq!(
        error,
        SupervisorPidRegistryError::ProcessNotRegistered(process)
    );
}

#[test]
fn pid_map_feeds_platform_kill_adapters_directly() {
    // Given: a registry with two mappings across two processes.
    let registry = SupervisorPidRegistry::new();
    let process_a = process_id(0x08);
    let process_b = process_id(0x09);
    registry
        .register(register_request(process_a, Generation::INITIAL, 4242))
        .expect("registration a");
    registry
        .register(register_request(process_b, Generation::INITIAL, 5151))
        .expect("registration b");

    // When: the registry snapshot feeds the adapter constructor (the exact
    // caller-injected `HashMap<ProcessId, u32>` shape both adapters accept).
    let pid_map: HashMap<ProcessId, u32> = registry.pid_map();
    let _posix = PosixPlatformKillAdapter::new(pid_map.clone());

    // Then: the snapshot shape matches the adapter contract.
    assert_eq!(
        pid_map,
        HashMap::from([(process_a, 4242), (process_b, 5151)])
    );
}
