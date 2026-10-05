//! Supervisor pid discovery / scan acceptance tests (W47-L1, the
//! `C-APP-CONTROL` #11 pid-discovery half).
//!
//! Under test, end to end over the authority's durable bindings:
//!
//! - the fresh / stale / missing classification of every recorded-pid
//!   state (live, dead, generation drift, unmapped) including
//!   resolver-supplied candidates (live and dead);
//! - scan idempotency and the zero-side-effect contract (registry,
//!   authority, and OS state untouched — the probe hook is the only
//!   thing asked);
//! - empty and all-terminal target sets (nothing to discover, empty
//!   report, no error);
//! - the interaction with the per-generation unregister half: scans never
//!   widen the unregister gate, and adoption never bypasses it;
//! - the opt-in adoption face: only fresh findings register, replays stay
//!   idempotent with the original `registered_at_ms`, and drift adoption
//!   supersedes;
//! - the real host probe classifies a live pid alive and a reaped child
//!   dead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_process::{
    CreateIsolationDomainRequest, HostSupervisorPidProbe, MarkProcessTerminatedRequest,
    ProcessAuthority, ProcessBindingRecord, ProcessSupervisor, RegisterDelegatedProcessRequest,
    RegisterSupervisorPidRequest, RestoreProcessRequest, SupervisorError, SupervisorPidDecision,
    SupervisorPidEntry, SupervisorPidFinding, SupervisorPidLiveness, SupervisorPidProbe,
    SupervisorPidScanTarget, SupervisorPidStaleReason,
};
use nlos_types::{Generation, IdempotencyKey, ProcessId, TaskAttemptId, TaskId};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-process-pid-scan-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The injected liveness-and-resolution hook: pids in `dead` (or `failing`)
/// answer dead (or error), everything else answers alive; `resolved` backs
/// the OS-side resolver.
#[derive(Default)]
struct StubProbe {
    dead: HashSet<u32>,
    failing: HashSet<u32>,
    resolved: HashMap<ProcessId, u32>,
}

impl SupervisorPidProbe for StubProbe {
    fn os_pid_liveness(&mut self, os_pid: u32) -> Result<SupervisorPidLiveness, SupervisorError> {
        if self.failing.contains(&os_pid) {
            return Err(SupervisorError::LivenessProbe("stub probe failure"));
        }
        if self.dead.contains(&os_pid) {
            Ok(SupervisorPidLiveness::Dead)
        } else {
            Ok(SupervisorPidLiveness::Alive)
        }
    }

    fn resolve_os_pid(&mut self, target: SupervisorPidScanTarget) -> Option<u32> {
        self.resolved.get(&target.process_id).copied()
    }
}

fn domain_request(seed: u8) -> CreateIsolationDomainRequest {
    CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000 + u64::from(seed),
    }
}

fn registration(
    seed: u8,
    domain: &nlos_process::IsolationDomainRecord,
) -> RegisterDelegatedProcessRequest {
    RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(6); 16]),
        created_at_ms: 2_000 + u64::from(seed),
    }
}

fn restore_request(binding: &ProcessBindingRecord, seed: u8) -> RestoreProcessRequest {
    RestoreProcessRequest {
        process_id: binding.process_id,
        expected_process_generation: binding.process_generation,
        expected_process_fencing_token: binding.process_fencing_token,
        isolation_domain_id: binding.isolation_domain_id,
        isolation_domain_generation: binding.isolation_domain_generation,
        isolation_domain_fencing_token: binding.isolation_domain_fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        restored_at_ms: 9_000,
    }
}

fn terminate_request(binding: &ProcessBindingRecord, seed: u8) -> MarkProcessTerminatedRequest {
    MarkProcessTerminatedRequest {
        process_id: binding.process_id,
        expected_process_generation: binding.process_generation,
        expected_process_fencing_token: binding.process_fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        marked_at_ms: 9_500,
    }
}

/// Registers `count` delegated processes under one domain and returns their
/// binding records (durable set persists in `root` for reopening).
fn registered_processes(root: &Path, domain_seed: u8, seeds: &[u8]) -> Vec<ProcessBindingRecord> {
    let authority = ProcessAuthority::open(root).expect("open authority");
    let domain = authority
        .create_isolation_domain(domain_request(domain_seed))
        .expect("create domain")
        .record()
        .clone();
    let mut records = Vec::new();
    for &seed in seeds {
        let decision = authority
            .register_delegated_process(registration(seed, &domain))
            .expect("register delegated process");
        records.push(decision.record().clone());
    }
    records
}

fn register_mapping(
    supervisor: &ProcessSupervisor,
    process_id: ProcessId,
    process_generation: Generation,
    os_pid: u32,
) -> SupervisorPidEntry {
    *supervisor
        .registry()
        .register(RegisterSupervisorPidRequest {
            process_id,
            process_generation,
            os_pid,
            registered_at_ms: 5_000,
        })
        .expect("register supervisor pid mapping")
        .current()
}

fn scan_targets(records: &[ProcessBindingRecord]) -> Vec<SupervisorPidScanTarget> {
    records.iter().map(SupervisorPidScanTarget::from).collect()
}

fn process_id_at(seed: u8) -> ProcessId {
    ProcessId::from_bytes([seed; 16])
}

#[test]
fn scan_classifies_live_recorded_pid_as_fresh() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x41);
    let recorded = register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };

    let mut probe = StubProbe::default();
    let report = supervisor
        .scan_pids(&[target], &mut probe)
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [SupervisorPidFinding::Fresh {
            target,
            os_pid: 4_242,
        }]
    );
    // Pure read: the recorded row is untouched.
    assert_eq!(supervisor.registry().lookup(process_id), Ok(recorded));
}

#[test]
fn scan_classifies_dead_recorded_pid_as_stale_without_consulting_the_resolver() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x42);
    let recorded = register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };

    // The resolver offers a live replacement; a same-generation dead pid
    // must not take it (one generation owns exactly one pid).
    let mut probe = StubProbe {
        dead: HashSet::from([4_242]),
        resolved: HashMap::from([(process_id, 5_151)]),
        ..StubProbe::default()
    };
    let report = supervisor
        .scan_pids(&[target], &mut probe)
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [SupervisorPidFinding::Stale {
            target,
            recorded: Some(recorded),
            reason: SupervisorPidStaleReason::RecordedPidDead,
        }]
    );
}

#[test]
fn scan_classifies_unmapped_target_as_missing() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x43);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };

    let mut probe = StubProbe::default();
    let report = supervisor
        .scan_pids(&[target], &mut probe)
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [SupervisorPidFinding::Missing { target }]
    );
    assert!(supervisor.registry().pid_map().is_empty());
}

#[test]
fn scan_reports_generation_drift_as_stale_without_a_resolver() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x44);
    let next = Generation::INITIAL.checked_next().expect("next");
    let recorded = register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);

    let mut probe = StubProbe::default();
    let report = supervisor
        .scan_pids(
            &[SupervisorPidScanTarget {
                process_id,
                process_generation: next,
            }],
            &mut probe,
        )
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [SupervisorPidFinding::Stale {
            target: SupervisorPidScanTarget {
                process_id,
                process_generation: next,
            },
            recorded: Some(recorded),
            reason: SupervisorPidStaleReason::GenerationDrift {
                recorded_generation: Generation::INITIAL,
            },
        }]
    );
}

#[test]
fn resolver_supplied_live_pid_freshens_missing_and_drifted_targets() {
    let supervisor = ProcessSupervisor::new();
    let missing_id = process_id_at(0x45);
    let drifted_id = process_id_at(0x46);
    let next = Generation::INITIAL.checked_next().expect("next");
    register_mapping(&supervisor, drifted_id, Generation::INITIAL, 4_242);

    let targets = [
        SupervisorPidScanTarget {
            process_id: missing_id,
            process_generation: Generation::INITIAL,
        },
        SupervisorPidScanTarget {
            process_id: drifted_id,
            process_generation: next,
        },
    ];
    let mut probe = StubProbe {
        resolved: HashMap::from([(missing_id, 6_161), (drifted_id, 7_171)]),
        ..StubProbe::default()
    };
    let report = supervisor
        .scan_pids(&targets, &mut probe)
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [
            SupervisorPidFinding::Fresh {
                target: targets[0],
                os_pid: 6_161,
            },
            SupervisorPidFinding::Fresh {
                target: targets[1],
                os_pid: 7_171,
            },
        ]
    );
    // Pure read: the drifted row stays at its recorded generation until an
    // explicit adoption supersedes it.
    assert_eq!(
        supervisor.registry().pid_map(),
        HashMap::from([(drifted_id, 4_242)])
    );
}

#[test]
fn resolver_supplied_dead_candidate_reports_stale() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x47);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };

    let mut probe = StubProbe {
        dead: HashSet::from([6_161]),
        resolved: HashMap::from([(process_id, 6_161)]),
        ..StubProbe::default()
    };
    let report = supervisor
        .scan_pids(&[target], &mut probe)
        .expect("scan must classify");

    assert_eq!(
        report.findings(),
        [SupervisorPidFinding::Stale {
            target,
            recorded: None,
            reason: SupervisorPidStaleReason::CandidatePidDead { candidate: 6_161 },
        }]
    );
}

/// The full authority-sourced pass: one fresh row, one drifted row (the
/// authority restored the process while the registry still maps the old
/// generation) — scanning twice reports equal findings and leaves the
/// registry, the durable set, and the lifecycle states untouched.
#[test]
fn rescan_over_durable_bindings_is_idempotent_with_zero_side_effects() {
    let root = TestRoot::new("rescan-idempotent");
    let records = registered_processes(root.path(), 0x10, &[0x21, 0x22]);
    let fresh_binding = &records[0];
    let drifted_binding = &records[1];

    // Restore the second process: the authority head advances while the
    // supervisor registry still maps the initial generation.
    let authority = ProcessAuthority::open(root.path()).expect("reopen authority");
    let restored = authority
        .restore_process(restore_request(drifted_binding, 0x31))
        .expect("restore process");
    let drifted_generation = restored.record().process_generation;

    let supervisor = ProcessSupervisor::new();
    let fresh_recorded = register_mapping(
        &supervisor,
        fresh_binding.process_id,
        Generation::INITIAL,
        4_242,
    );
    let drifted_recorded = register_mapping(
        &supervisor,
        drifted_binding.process_id,
        Generation::INITIAL,
        5_151,
    );

    let mut probe = StubProbe::default();
    let targets = scan_targets(&authority.list_active_process_bindings().expect("list"));
    let first = supervisor
        .scan_pids(&targets, &mut probe)
        .expect("first scan");
    let second = supervisor
        .scan_pids(&targets, &mut probe)
        .expect("second scan");

    assert_eq!(
        first, second,
        "a rescan with unchanged answers must be equal"
    );
    let by_process: HashMap<ProcessId, SupervisorPidFinding> = first
        .findings()
        .iter()
        .map(|finding| (finding.target().process_id, *finding))
        .collect();
    assert_eq!(
        by_process.get(&fresh_binding.process_id),
        Some(&SupervisorPidFinding::Fresh {
            target: SupervisorPidScanTarget {
                process_id: fresh_binding.process_id,
                process_generation: Generation::INITIAL,
            },
            os_pid: 4_242,
        }),
        "the live recorded row at the head generation is fresh"
    );
    assert_eq!(
        by_process.get(&drifted_binding.process_id),
        Some(&SupervisorPidFinding::Stale {
            target: SupervisorPidScanTarget {
                process_id: drifted_binding.process_id,
                process_generation: drifted_generation,
            },
            recorded: Some(drifted_recorded),
            reason: SupervisorPidStaleReason::GenerationDrift {
                recorded_generation: Generation::INITIAL,
            },
        }),
        "the row mapped at the pre-restore generation is stale drift"
    );

    // Zero side effects: registry rows and the durable listing are unchanged.
    assert_eq!(
        supervisor.registry().lookup(fresh_binding.process_id),
        Ok(fresh_recorded)
    );
    assert_eq!(
        supervisor.registry().lookup(drifted_binding.process_id),
        Ok(drifted_recorded)
    );
    assert_eq!(
        authority
            .list_active_process_bindings()
            .expect("list after scans")
            .len(),
        2
    );
    assert!(
        authority
            .inspect_process_terminal(fresh_binding.process_id)
            .expect("terminal inspect")
            .is_none()
    );
}

#[test]
fn scan_over_empty_and_all_terminal_sets_is_empty() {
    let root = TestRoot::new("empty-terminal");

    // Empty durable set: nothing to discover, empty report, no error.
    let supervisor = ProcessSupervisor::new();
    let mut probe = StubProbe::default();
    let report = supervisor
        .scan_pids(&[], &mut probe)
        .expect("empty target scan");
    assert!(report.is_empty());
    assert!(matches!(report.findings(), []));

    // All-terminal durable set: terminal heads are not scan targets.
    let records = registered_processes(root.path(), 0x11, &[0x24, 0x25]);
    let authority = ProcessAuthority::open(root.path()).expect("reopen authority");
    for (binding, seed) in records.iter().zip([0x34u8, 0x35]) {
        authority
            .mark_process_terminated(terminate_request(binding, seed))
            .expect("mark terminated");
    }
    let targets = scan_targets(&authority.list_active_process_bindings().expect("list"));
    assert!(
        targets.is_empty(),
        "terminal heads must not be scan targets"
    );
    let report = supervisor
        .scan_pids(&targets, &mut probe)
        .expect("all-terminal scan");
    assert!(report.is_empty());
    assert!(matches!(report.findings(), []));
}

#[test]
fn scan_fails_closed_when_the_probe_errors() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x48);
    let recorded = register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };

    let mut probe = StubProbe {
        failing: HashSet::from([4_242]),
        ..StubProbe::default()
    };
    let error = supervisor
        .scan_pids(&[target], &mut probe)
        .expect_err("a failing probe must fail the scan closed");

    assert!(
        matches!(error, SupervisorError::LivenessProbe(_)),
        "expected the typed liveness-probe failure, got {error:?}"
    );
    assert_eq!(
        supervisor.registry().lookup(process_id),
        Ok(recorded),
        "the failed scan must leave the mapping untouched"
    );
}

#[test]
fn list_active_process_bindings_excludes_terminal_heads_and_orders_by_identity() {
    let root = TestRoot::new("list-active");
    let records = registered_processes(root.path(), 0x12, &[0x26, 0x27, 0x28]);
    let authority = ProcessAuthority::open(root.path()).expect("reopen authority");
    authority
        .mark_process_terminated(terminate_request(&records[1], 0x33))
        .expect("mark the middle binding terminal");

    let listed = authority
        .list_active_process_bindings()
        .expect("list active bindings");

    let mut expected = vec![records[0].clone(), records[2].clone()];
    expected.sort_by_key(|record| *record.process_id.as_bytes());
    assert_eq!(listed, expected);
    let listed_target = SupervisorPidScanTarget::from(&listed[0]);
    assert_eq!(
        listed_target,
        SupervisorPidScanTarget::from(&expected[0]),
        "the target conversion carries the identity and head generation"
    );
}

/// The discovery half must not widen or break the per-generation unregister
/// half (C-APP-CONTROL #11): scans observe, unregister still removes by
/// exact generation, and re-registering after an unregister stays the
/// caller's explicit path.
#[test]
fn scan_interplays_with_per_generation_unregister() {
    let supervisor = ProcessSupervisor::new();
    let process_id = process_id_at(0x49);
    let target = SupervisorPidScanTarget {
        process_id,
        process_generation: Generation::INITIAL,
    };
    let mut probe = StubProbe::default();

    // Fresh → unregister → missing → re-register → fresh again.
    register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);
    assert!(matches!(
        supervisor
            .scan_pids(&[target], &mut probe)
            .expect("scan")
            .findings(),
        [SupervisorPidFinding::Fresh { .. }]
    ));
    assert!(
        supervisor
            .unregister(process_id, Generation::INITIAL)
            .expect("unregister")
    );
    assert_eq!(
        supervisor
            .scan_pids(&[target], &mut probe)
            .expect("rescan")
            .findings(),
        [SupervisorPidFinding::Missing { target }]
    );
    let re_entry = register_mapping(&supervisor, process_id, Generation::INITIAL, 4_242);
    assert_eq!(
        supervisor.registry().lookup(process_id),
        Ok(re_entry),
        "re-registering after unregister is the caller's explicit path"
    );
    assert!(matches!(
        supervisor
            .scan_pids(&[target], &mut probe)
            .expect("scan after re-register")
            .findings(),
        [SupervisorPidFinding::Fresh { .. }]
    ));
}

/// Drifted rows unregister at their own recorded generation, and an
/// adoption-superseded row keeps the stale-generation unregister gate: the
/// discovery half never widens the unregister fence.
#[test]
fn drifted_rows_unregister_at_their_generation_and_adoption_keeps_the_stale_gate() {
    let supervisor = ProcessSupervisor::new();
    let next = Generation::INITIAL.checked_next().expect("next");
    let mut probe = StubProbe::default();

    // Drift → unregister the drifted row → missing.
    let drifted_id = process_id_at(0x4a);
    let drifted_target = SupervisorPidScanTarget {
        process_id: drifted_id,
        process_generation: next,
    };
    register_mapping(&supervisor, drifted_id, Generation::INITIAL, 5_151);
    assert_eq!(
        supervisor
            .scan_pids(&[drifted_target], &mut probe)
            .expect("drift scan")
            .findings(),
        [SupervisorPidFinding::Stale {
            target: drifted_target,
            recorded: supervisor.registry().lookup(drifted_id).ok(),
            reason: SupervisorPidStaleReason::GenerationDrift {
                recorded_generation: Generation::INITIAL,
            },
        }]
    );
    assert!(
        supervisor
            .unregister(drifted_id, Generation::INITIAL)
            .expect("unregister the drifted row at its own generation")
    );
    assert_eq!(
        supervisor
            .scan_pids(&[drifted_target], &mut probe)
            .expect("rescan after unregister")
            .findings(),
        [SupervisorPidFinding::Missing {
            target: drifted_target
        }]
    );

    // Adoption path: a resolver-freshened finding supersedes the drifted
    // row; unregistering the superseded generation still fails closed.
    let adopt_id = process_id_at(0x4b);
    let adopt_target = SupervisorPidScanTarget {
        process_id: adopt_id,
        process_generation: next,
    };
    register_mapping(&supervisor, adopt_id, Generation::INITIAL, 6_161);
    let mut resolving_probe = StubProbe {
        resolved: HashMap::from([(adopt_id, 7_171)]),
        ..StubProbe::default()
    };
    let report = supervisor
        .scan_pids(&[adopt_target], &mut resolving_probe)
        .expect("scan for adoption");
    assert!(matches!(
        report.findings(),
        [SupervisorPidFinding::Fresh { .. }]
    ));
    supervisor
        .adopt_fresh_findings(&report, 8_000)
        .expect("adopt the fresh finding");
    assert!(
        matches!(
            supervisor.unregister(adopt_id, Generation::INITIAL),
            Err(SupervisorError::Registry(
                nlos_process::SupervisorPidRegistryError::StaleProcessGeneration { .. }
            ))
        ),
        "the stale-generation unregister gate must survive adoption"
    );
    assert!(
        supervisor
            .unregister(adopt_id, next)
            .expect("unregister at the adopted generation")
    );
}

#[test]
fn adopt_fresh_findings_registers_only_fresh_rows_and_replays_idempotently() {
    let supervisor = ProcessSupervisor::new();
    let fresh_recorded_id = process_id_at(0x4c);
    let fresh_discovered_id = process_id_at(0x4d);
    let missing_id = process_id_at(0x4e);
    let stale_id = process_id_at(0x4f);

    let original = register_mapping(&supervisor, fresh_recorded_id, Generation::INITIAL, 4_242);
    register_mapping(&supervisor, stale_id, Generation::INITIAL, 8_181);

    let targets =
        [fresh_recorded_id, fresh_discovered_id, missing_id, stale_id].map(|process_id| {
            SupervisorPidScanTarget {
                process_id,
                process_generation: Generation::INITIAL,
            }
        });
    let mut probe = StubProbe {
        dead: HashSet::from([8_181]),
        resolved: HashMap::from([(fresh_discovered_id, 5_151)]),
        ..StubProbe::default()
    };
    let report = supervisor
        .scan_pids(&targets, &mut probe)
        .expect("mixed scan");
    assert_eq!(report.findings().len(), 4);

    let decisions = supervisor
        .adopt_fresh_findings(&report, 9_000)
        .expect("adopt fresh findings");
    assert_eq!(
        supervisor.registry().pid_map(),
        HashMap::from([
            (fresh_recorded_id, 4_242),
            (fresh_discovered_id, 5_151),
            (stale_id, 8_181),
        ]),
        "adoption registers the discovered row and leaves stale/missing rows alone"
    );

    // Replaying the same report is idempotent and preserves the original
    // registration timestamp (a replay, not a rebind).
    let replayed = supervisor
        .adopt_fresh_findings(&report, 9_999)
        .expect("re-adopt fresh findings");
    assert_eq!(decisions.len(), 2);
    assert_eq!(replayed.len(), 2);
    assert!(matches!(decisions[0], SupervisorPidDecision::Replayed(_)));
    assert!(matches!(decisions[1], SupervisorPidDecision::Registered(_)));
    assert_eq!(
        supervisor.registry().lookup(fresh_recorded_id),
        Ok(original),
        "the recorded row keeps its original entry through adoption replays"
    );

    // An empty report adopts nothing.
    let empty_report = supervisor.scan_pids(&[], &mut probe).expect("empty scan");
    let adopted = supervisor
        .adopt_fresh_findings(&empty_report, 9_000)
        .expect("adopt from an empty report");
    assert!(matches!(adopted.as_slice(), []));
}

/// The real host probe: the scanning process's own pid is alive, and a
/// spawned-then-reaped child's pid is dead (the null-signal existence
/// check observes real OS state, no signal is delivered).
#[test]
#[cfg(unix)]
fn host_probe_classifies_own_pid_alive_and_reaped_child_dead() {
    let mut probe = HostSupervisorPidProbe;
    assert_eq!(
        probe
            .os_pid_liveness(std::process::id())
            .expect("probe own pid"),
        SupervisorPidLiveness::Alive
    );

    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn sleeper");
    let child_pid = child.id();
    child.kill().expect("kill sleeper");
    child.wait().expect("reap sleeper");
    assert_eq!(
        probe.os_pid_liveness(child_pid).expect("probe reaped pid"),
        SupervisorPidLiveness::Dead
    );
}

/// Windows arm of the host probe acceptance: the probing process's own pid
/// is alive, a spawned-then-terminated child's pid is dead.
#[test]
#[cfg(windows)]
fn host_probe_classifies_own_pid_alive_and_reaped_child_dead() {
    let mut probe = HostSupervisorPidProbe;
    assert_eq!(
        probe
            .os_pid_liveness(std::process::id())
            .expect("probe own pid"),
        SupervisorPidLiveness::Alive
    );

    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 600"])
        .spawn()
        .expect("spawn sleeper");
    let child_pid = child.id();
    child.kill().expect("kill sleeper");
    child.wait().expect("reap sleeper");
    assert_eq!(
        probe.os_pid_liveness(child_pid).expect("probe reaped pid"),
        SupervisorPidLiveness::Dead
    );
}
