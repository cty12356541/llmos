//! W59-1 daemon executor-arm tests (`daemon` feature): the four
//! authority-backed executors as actually assembled inside the resident
//! daemon —
//!
//! - **kill** (Unix): a real seeded OS child is registered in the
//!   daemon-owned supervisor pid registry, one `kill operation` through
//!   the daemon's own dispatch face signals the real child (it dies by
//!   signal), the durable platform-kill receipt commits in the daemon's
//!   process authority, and a replay re-derives the identical receipt id.
//!   An unregistered target refuses typed `NOT_FOUND` — the honest
//!   production posture of the missing supervisor registration surface;
//! - **throttle** (portable): one `throttle operation` records a decision
//!   in the W44-SC1 durable ledger under the daemon's resource root, a
//!   daemon restart replays the same command byte-equal from the ledger,
//!   and a second, different command composes on the decision chain;
//! - **reclaim** (portable): the occupancy observation is the live task
//!   authority's own issued-permit count (461 seeded permits, above the
//!   10K tier soft threshold), a stale CAS expectation refuses typed, and
//!   the executed advisory replays the identical receipt id;
//! - **application uninstall** (portable): one `uninstall application`
//!   crosses the W27-D task-activity gate over the daemon's own task
//!   authority and leaves the terminal durable mark.

#![cfg(feature = "daemon")]

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_system_control::control::{ControlCommand, ControlOutcome};
use nlos_system_control::daemon::{DaemonOptions, assemble};
use nlos_types::{Generation, IdempotencyKey, TaskAttemptId, TaskId};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// Short-lived temp root (socket paths must stay under the macOS
/// `SUN_LEN` bound on Unix).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-daemon-exe-{label}-{}-{sequence}",
            std::process::id(),
        )));
        // Pre-seeding opens authorities under the root before `assemble`
        // creates it, so the directory exists up front.
        fs::create_dir_all(&root.0).expect("create temp root");
        root
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Endpoint path in the host OS form: a `.sock` file under the temp dir on
/// Unix (short enough for the macOS `SUN_LEN` bound — the daemon's
/// root-derived defaults are not), a machine-local pipe name on Windows.
#[cfg(unix)]
fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-daemon-exe-{label}-{suffix}-{}-{sequence}.sock",
        std::process::id(),
    ))
}

/// Endpoint path in the host OS form: a `.sock` file under the temp dir on
/// Unix (short enough for the macOS `SUN_LEN` bound — the daemon's
/// root-derived defaults are not), a machine-local pipe name on Windows.
#[cfg(windows)]
fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-daemon-exe-{label}-{suffix}-{}-{sequence}",
        std::process::id(),
    ))
}

/// One durable delegated-process binding under the daemon's process root
/// (the same authority path `assemble` reopens), mirroring the W29-D
/// fixture shape.
#[cfg(unix)]
fn process_fixture(
    authority: &nlos_process::ProcessAuthority,
    seed: u8,
) -> nlos_process::ProcessBindingRecord {
    use nlos_process::{CreateIsolationDomainRequest, RegisterDelegatedProcessRequest};

    let domain = authority
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed; 32],
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record()
        .clone();
    authority
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed; 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: domain.isolation_domain_id,
            isolation_domain_generation: domain.generation,
            isolation_domain_fencing_token: domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
            created_at_ms: 1_100,
        })
        .unwrap()
        .record()
        .clone()
}

/// One activated reservation with a single consumption under the daemon's
/// resource root, so the wire CAS (`usage_high_water_seq`) addresses
/// revision 1. Mirrors the W29-D/W44-SC1 fixture shape.
fn demand_fixture(root: &std::path::Path, seed: u8) -> nlos_resource::ReservationRecord {
    use nlos_resource::{
        ActivateReservationRequest, ConsumeReservationRequest, CreateAccountRequest,
        CreateQuoteRequest, RegisterDriverRequest, ReserveRequest, ResourceAuthority,
        ResourceDemand,
    };
    use nlos_types::{CallId, OperationId};

    let authority = ResourceAuthority::open(root.join("resource")).unwrap();
    let driver = authority
        .register_driver(RegisterDriverRequest {
            profile_digest: [seed; 32],
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let account = authority
        .create_account(CreateAccountRequest {
            initial_credit: 1_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    let capacity = ResourceDemand {
        cpu_shares: 100,
        memory_mib: 1_024,
        io_weight: 10,
    };
    let quote = authority
        .create_quote(CreateQuoteRequest {
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            operation_proposal_digest: [seed.wrapping_add(3); 32],
            pricing_version: [seed.wrapping_add(4); 32],
            upper_bound: 100,
            demand_capacity: capacity,
            valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let reservation = authority
        .reserve(ReserveRequest {
            account_id: account.account_id,
            quote_id: quote.quote_id,
            call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
            operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
            demand: ResourceDemand {
                cpu_shares: 64,
                memory_mib: 512,
                io_weight: 5,
            },
            reserved_at_ms: 2_000,
        })
        .unwrap()
        .record();
    let activation = authority
        .activate(ActivateReservationRequest {
            reservation_id: reservation.reservation_id,
            call_id: reservation.call_id,
            operation_id: reservation.operation_id,
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            activation_token: reservation.activation_token,
            activated_at_ms: 2_500,
        })
        .unwrap()
        .receipt();
    authority
        .consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: activation.receipt_id,
            sequence: 1,
            cumulative_usage: 10,
            consumed_at_ms: 3_000,
        })
        .unwrap();
    authority
        .inspect_reservation(reservation.reservation_id)
        .unwrap()
}

/// Registers one task + attempt and issues one outstanding `CommitPermit`
/// through the public task-authority faces (the same minimal chain the
/// recovery-control harness uses), so the store-wide issued-permit count —
/// the working-set occupancy the daemon's reclaim arm observes — advances
/// by exactly one.
fn issue_permit(tasks: &nlos_task::SqliteTaskAuthority, index: u16) {
    use nlos_task::{
        ArtifactPublicationExpectation, AttemptSpec, Authorities, PermitDecision, PermitRequest,
        SnapshotBundle, TaskSpec, artifact_publication_plan_root, empty_effect_history_root,
    };
    use nlos_types::{ArtifactId, CancellationScopeId, TaskSnapshotId};

    let mut task_bytes = [0xB1_u8; 16];
    task_bytes[..2].copy_from_slice(&index.to_be_bytes());
    let task_id = TaskId::from_bytes(task_bytes);
    let mut attempt_bytes = [0xB2_u8; 16];
    attempt_bytes[..2].copy_from_slice(&index.to_be_bytes());
    let attempt_id = TaskAttemptId::from_bytes(attempt_bytes);
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .unwrap();
    tasks
        .register_attempt(AttemptSpec {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes([0xB3; 16]),
                snapshot_digest: [0xB4; 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([0xB5; 16]),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: IdempotencyKey::from_bytes([0xB6; 16]),
            registered_at_ms: 2_000,
        })
        .unwrap();
    let expectation = ArtifactPublicationExpectation {
        staging_id: [0xB7; 16],
        artifact_id: ArtifactId::from_bytes([0xB8; 16]),
        target_revision: 1,
        digest: [0xB9; 32],
        size_bytes: 10,
    };
    match tasks
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: artifact_publication_plan_root(std::slice::from_ref(&expectation))
                    .unwrap(),
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0xBA; 16]),
                valid_until_ms: 20_000,
                requested_at_ms: 3_000,
            },
        )
        .unwrap()
    {
        PermitDecision::Issued(_) => {}
        other => panic!("expected permit {index} to issue, got {other:?}"),
    }
}

/// One installed application (generation 1, `installed`) over the daemon's
/// own root layout: artifacts under `<root>/artifacts`, identity under
/// `<root>/identity`, the application row under `<root>/application` —
/// exactly the paths `assemble` reopens. Mirrors the W35-P11 fixture.
fn install_application_fixture(root: &std::path::Path, seed: u8) -> nlos_types::PackageId {
    use ed25519_dalek::Signer as _;
    use nlos_application::{ApplicationAuthority, InstallApplicationRequest, InstallDecision};
    use nlos_artifact::{
        ArtifactStore, ContentDigest, CreateArtifactSpec, PackageEntryRole, PackageManifest,
        PackageManifestEntry, ProvenanceSourceTriple, PutRevisionRequest, SignedPackage,
        VerifyPackageRequest, package_manifest_message,
    };
    use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
    use nlos_types::ArtifactId;

    let artifacts = ArtifactStore::open(root.join("artifacts")).unwrap();
    let identity = IdentityAuthority::open(root.join("identity")).unwrap();
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let bootstrap = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
        .binding();
    let artifact_id = ArtifactId::from_bytes([0x30; 16]);
    let payload: &[u8] = b"payload-of-the-package";
    artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0xA0_u8.wrapping_add(seed); 16]),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: Some(format!("user-{seed}")),
            created_at_ms: 1_000,
        })
        .unwrap();
    artifacts
        .put_revision(PutRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: payload,
            created_at_ms: 5_000,
            provenance: ProvenanceSourceTriple {
                source_a: [0xC0_u8.wrapping_add(seed); 16],
                source_b: [0xD0_u8.wrapping_add(seed); 16],
                source_digest: ContentDigest::from_bytes([0xE0_u8.wrapping_add(seed); 32]),
            },
        })
        .unwrap();
    let package_id = nlos_types::PackageId::from_bytes([seed; 16]);
    let manifest = PackageManifest {
        package_id,
        version: 1,
        entries: vec![PackageManifestEntry {
            name: "main".to_string(),
            artifact_id,
            digest: ContentDigest::of_bytes(payload),
            role: PackageEntryRole::Executable,
        }],
    };
    let signed = SignedPackage {
        manifest: manifest.clone(),
        signer: bootstrap.principal_id,
        signature: key.sign(&package_manifest_message(&manifest)).to_bytes(),
    };
    let verified = artifacts
        .verify_package(
            &identity,
            VerifyPackageRequest {
                signed: &signed,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
                verified_at_ms: 1_000,
            },
        )
        .unwrap();
    let applications = ApplicationAuthority::open(root.join("application")).unwrap();
    let InstallDecision::Installed(receipt) = applications
        .install_application(
            &artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: verified.receipt().receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
                installed_at_ms: 2_000,
            },
        )
        .unwrap()
    else {
        panic!("fresh key must install");
    };
    assert_eq!(receipt.installation_generation, Generation::INITIAL);
    package_id
}

/// The daemon's own dispatch face, at one fixed clock pair so replays
/// reconstruct byte-identical executor requests.
fn dispatch(
    daemon: &nlos_system_control::daemon::SystemControlDaemon,
    command: &ControlCommand,
) -> nlos_system_control::control::ControlReceipt {
    daemon
        .dispatch_command(command, 10, 6_000)
        .expect("dispatch")
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_signals_a_registered_real_child_and_replays_idempotently() {
    use nlos_process::{ProcessAuthority, RegisterSupervisorPidRequest};
    use std::os::unix::process::ExitStatusExt as _;

    let root = TempRoot::new("kill");
    let auth_socket = temp_path("kill", "auth");
    let plain_socket = temp_path("kill", "plain");
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn real child");

    // Seed two durable process bindings under the daemon's process root
    // and hand the handle back to nothing — the daemon reopens the path.
    let seeding = ProcessAuthority::open(root.0.join("process")).unwrap();
    let registered = process_fixture(&seeding, 0x41);
    let unregistered = process_fixture(&seeding, 0x42);
    drop(seeding);

    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    // The honest production posture of the missing supervisor
    // registration surface: an unregistered target refuses typed before
    // the process authority is driven.
    let refused = dispatch(
        &daemon,
        &ControlCommand::KillOperation {
            control_command_id: unregistered.process_id.into_bytes(),
            target_id: unregistered.process_id.into_bytes(),
            expected_generation_or_revision: unregistered.process_generation.get(),
            reason: "no supervisor mapping is registered".to_owned(),
        },
    );
    let failure = refused.outcome.expect_err("unregistered kill must refuse");
    assert_eq!(
        failure.safe_message,
        "no supervisor os pid mapping is registered for the process"
    );
    let reopened = ProcessAuthority::open(root.0.join("process")).unwrap();
    assert!(
        reopened
            .inspect_platform_kill_receipt(unregistered.process_id, unregistered.process_generation)
            .unwrap()
            .is_none(),
        "the refused kill took no durable side effect"
    );
    drop(reopened);

    // Register the real child; the per-exchange pid-map snapshot observes
    // the registration on the very next dispatch.
    daemon
        .register_supervisor_pid(RegisterSupervisorPidRequest {
            process_id: registered.process_id,
            process_generation: registered.process_generation,
            os_pid: child.id(),
            registered_at_ms: 1_200,
        })
        .expect("register supervisor pid");

    let kill = ControlCommand::KillOperation {
        control_command_id: registered.process_id.into_bytes(),
        target_id: registered.process_id.into_bytes(),
        expected_generation_or_revision: registered.process_generation.get(),
        reason: "operator kills the delegated process".to_owned(),
    };
    let receipt = dispatch(&daemon, &kill);
    let ControlOutcome::OperationKilled { receipt_id } = receipt.outcome.expect("kill outcome")
    else {
        panic!("expected an operation-killed receipt");
    };

    // The signal really went out: the OS child died by signal.
    let status = child.wait().expect("wait for the killed child");
    assert!(!status.success(), "the real OS child died by signal");
    assert!(status.signal().is_some(), "death was a signal, not exit");

    // The durable platform-kill receipt committed in the daemon's process
    // authority under the command identity (the target id).
    let authority = ProcessAuthority::open(root.0.join("process")).unwrap();
    let durable = authority
        .inspect_platform_kill_receipt(registered.process_id, registered.process_generation)
        .unwrap()
        .expect("durable platform kill receipt");
    assert_eq!(
        durable.idempotency_key.as_bytes(),
        registered.process_id.as_bytes()
    );
    drop(authority);

    // Replay through a fresh handler stack: the same receipt id
    // re-derives from the durable receipt (at-least-once signal delivery
    // re-signals the already-dead child — ESRCH maps to success).
    let replay = dispatch(&daemon, &kill);
    let ControlOutcome::OperationKilled {
        receipt_id: replayed,
    } = replay.outcome.expect("kill replay outcome")
    else {
        panic!("expected a replayed operation-killed receipt");
    };
    assert_eq!(receipt_id, replayed);

    daemon.stop_worker();
    drop(endpoints);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn throttle_decisions_survive_a_daemon_restart_and_replay() {
    use nlos_resource::ResourceAuthority;

    let root = TempRoot::new("throttle");
    let reservation = demand_fixture(&root.0, 0x42);
    assert_eq!(reservation.usage_high_water_seq, 1);

    let auth_socket = temp_path("throttle", "auth");
    let plain_socket = temp_path("throttle", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let command = ControlCommand::ThrottleOperation {
        control_command_id: reservation.reservation_id.into_bytes(),
        target_id: reservation.reservation_id.into_bytes(),
        expected_generation_or_revision: reservation.usage_high_water_seq,
        throttle_percent: 50,
        reason: "operator throttles the reservation demand".to_owned(),
    };
    let receipt = dispatch(&daemon, &command);
    let ControlOutcome::OperationThrottled { receipt_id } =
        receipt.outcome.expect("throttle outcome")
    else {
        panic!("expected an operation-throttled receipt");
    };

    // The W44-SC1 decision ledger is durable under the daemon's resource
    // root and folds into the effective-demand read face.
    let resource = ResourceAuthority::open(root.0.join("resource")).unwrap();
    let effective = resource
        .inspect_effective_demand(reservation.reservation_id)
        .unwrap();
    assert_eq!(effective.decisions.len(), 1);
    assert_eq!(effective.decisions[0].throttle_percent, 50);
    assert_eq!(
        (
            effective.effective_demand.cpu_shares,
            effective.effective_demand.memory_mib,
            effective.effective_demand.io_weight
        ),
        (32, 256, 2),
        "50% of the declared 64/512/5 demand"
    );
    drop(resource);

    // Daemon restart over the same root: the same command replays from
    // the ledger byte-equal (a different wall clock is irrelevant to the
    // recorded transition).
    daemon.stop_worker();
    daemon.stop_materialization();
    drop(endpoints);
    drop(daemon);
    let auth_socket = temp_path("throttle", "auth2");
    let plain_socket = temp_path("throttle", "plain2");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("reassemble daemon over the same root");
    let replay = daemon
        .dispatch_command(&command, 10, 7_000)
        .expect("replay dispatch");
    let ControlOutcome::OperationThrottled {
        receipt_id: replayed,
    } = replay.outcome.expect("throttle replay outcome")
    else {
        panic!("expected a replayed operation-throttled receipt");
    };
    assert_eq!(receipt_id, replayed);

    // A different command identity composes on the decision chain instead
    // of re-deriving from the declared demand.
    let second = ControlCommand::ThrottleOperation {
        control_command_id: [0x5A; 16],
        target_id: reservation.reservation_id.into_bytes(),
        expected_generation_or_revision: reservation.usage_high_water_seq,
        throttle_percent: 50,
        reason: "operator throttles the reservation demand again".to_owned(),
    };
    let receipt = dispatch(&daemon, &second);
    let ControlOutcome::OperationThrottled {
        receipt_id: chained,
    } = receipt.outcome.expect("second throttle outcome")
    else {
        panic!("expected a second operation-throttled receipt");
    };
    assert_ne!(receipt_id, chained);
    let resource = ResourceAuthority::open(root.0.join("resource")).unwrap();
    let effective = resource
        .inspect_effective_demand(reservation.reservation_id)
        .unwrap();
    assert_eq!(effective.decisions.len(), 2);
    assert_eq!(
        (
            effective.effective_demand.cpu_shares,
            effective.effective_demand.memory_mib,
            effective.effective_demand.io_weight
        ),
        (16, 128, 1),
        "the second 50% composes on the first decision's effective demand"
    );

    daemon.stop_worker();
    drop(endpoints);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reclaim_observes_the_live_task_authority_occupancy() {
    use nlos_task::SqliteTaskAuthority;

    let root = TempRoot::new("reclaim");
    // 461 outstanding permits: above the 10K tier's soft threshold
    // (`512 * 90% = 460`) and at/below its hard cap, so the pressure
    // snapshot carries a reclaim advisory.
    let tasks = SqliteTaskAuthority::open(root.0.join("tasks.sqlite3")).unwrap();
    for index in 0..461_u16 {
        issue_permit(&tasks, index);
    }
    drop(tasks);

    let auth_socket = temp_path("reclaim", "auth");
    let plain_socket = temp_path("reclaim", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let command = ControlCommand::ReclaimOperation {
        control_command_id: [0x71; 16],
        target_id: [0x71; 16],
        expected_generation_or_revision: 461,
        reason: "operator reclaims the working set".to_owned(),
    };
    let receipt = dispatch(&daemon, &command);
    let ControlOutcome::OperationReclaimed { receipt_id } =
        receipt.outcome.expect("reclaim outcome")
    else {
        panic!("expected an operation-reclaimed receipt");
    };

    // Replay re-derives the identical receipt (the executed prefix is the
    // synthetic RebuildableCache stand-in; it claims no durable eviction).
    let replay = dispatch(&daemon, &command);
    let ControlOutcome::OperationReclaimed {
        receipt_id: replayed,
    } = replay.outcome.expect("reclaim replay outcome")
    else {
        panic!("expected a replayed operation-reclaimed receipt");
    };
    assert_eq!(receipt_id, replayed);

    // The occupancy observation is the live authority's own count: a
    // stale CAS expectation refuses typed against the observed 461, not
    // against any host-side fixture constant.
    let stale = ControlCommand::ReclaimOperation {
        control_command_id: [0x72; 16],
        target_id: [0x72; 16],
        expected_generation_or_revision: 460,
        reason: "stale occupancy expectation".to_owned(),
    };
    let refused = dispatch(&daemon, &stale);
    let failure = refused.outcome.expect_err("stale CAS must refuse typed");
    assert_eq!(
        failure.safe_message,
        "reclaim CAS mismatch: the observed working-set count moved"
    );

    daemon.stop_worker();
    drop(endpoints);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uninstall_crosses_the_activity_gate_through_daemon_dispatch() {
    use nlos_application::{ApplicationAuthority, ApplicationStatus};

    let root = TempRoot::new("app");
    let package_id = install_application_fixture(&root.0, 0x61);

    let auth_socket = temp_path("app", "auth");
    let plain_socket = temp_path("app", "plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    let command = ControlCommand::UninstallApplication {
        control_command_id: package_id.into_bytes(),
        package_id: package_id.into_bytes(),
        expected_generation_or_revision: Generation::INITIAL.get(),
        reason: "operator uninstalls the application".to_owned(),
    };
    let receipt = dispatch(&daemon, &command);
    let ControlOutcome::ApplicationUninstalled { receipt_id } =
        receipt.outcome.expect("uninstall outcome")
    else {
        panic!("expected an application-uninstalled receipt");
    };

    // Replay through a fresh handler stack re-derives the same receipt id
    // from the authority's own idempotency.
    let replay = dispatch(&daemon, &command);
    let ControlOutcome::ApplicationUninstalled {
        receipt_id: replayed,
    } = replay.outcome.expect("uninstall replay outcome")
    else {
        panic!("expected a replayed application-uninstalled receipt");
    };
    assert_eq!(receipt_id, replayed);

    // The terminal mark is durable in the authority the daemon owns
    // (crossed the W27-D gate: no background-task activity existed).
    let applications = ApplicationAuthority::open(root.0.join("application")).unwrap();
    let application = applications
        .inspect_application(package_id)
        .unwrap()
        .expect("application row survives the daemon");
    assert!(matches!(application.status, ApplicationStatus::Uninstalled));

    daemon.stop_worker();
    drop(endpoints);
}
