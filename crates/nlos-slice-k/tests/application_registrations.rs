//! B-SLICE-K-001 ROAD-B-002 registration inspect: background-task and
//! process-binding registration wired through the slice assembler with
//! readback via [`ApplicationRegistrationInspect`].

use nlos_application::{
    ActiveTaskActivityProbe, ApplicationAuthorityError, UninstallApplicationRequest,
};
use nlos_slice_k::{SliceKRuntime, fixture_bytes, seeded_key};
use nlos_types::{ProcessId, TaskId};

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-{name}-{}-{sequence}",
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
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

/// Caller-supplied outstanding-task count for the activity-gated uninstall
/// path. Slice-K tests set the count from a prior `inspect_application_registrations`
/// read — the probe must not re-enter `ApplicationAuthority` while the gate
/// holds the writer lock.
struct FixedTaskProbe {
    count: u64,
}

impl ActiveTaskActivityProbe for FixedTaskProbe {
    fn outstanding_task_count(&self, _package_id: nlos_types::PackageId) -> u64 {
        self.count
    }
}

#[test]
fn register_background_task_and_process_binding_then_inspect_readback() {
    let dir = TempDir::new("application-registrations");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let seed = 0xF0_u8;
    let publisher = runtime.bootstrap_publisher(seed).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, seed, &fixture_bytes(seed, 48))
        .expect("publish");
    let verification = runtime
        .verify_signed_package(&package, seed)
        .expect("verify");
    runtime
        .install_verified_package(&verification, seed)
        .expect("install");

    let task_id = TaskId::from_bytes([seed.wrapping_add(40); 16]);
    let process_id = ProcessId::from_bytes([seed.wrapping_add(41); 16]);

    let background = runtime
        .register_background_task(package.package_id, task_id, publisher.principal_id, seed)
        .expect("register background task");
    assert_eq!(background.task_id, task_id);

    let binding = runtime
        .register_process_binding(package.package_id, process_id, publisher.principal_id, seed)
        .expect("register process binding");
    assert_eq!(binding.process_id, process_id);

    let inspect = runtime
        .inspect_application_registrations(package.package_id)
        .expect("inspect registrations");
    assert_eq!(inspect.background_tasks.len(), 1);
    assert_eq!(inspect.process_bindings.len(), 1);
    assert_eq!(inspect.background_tasks[0], background);
    assert_eq!(inspect.process_bindings[0], binding);

    let replay_background = runtime
        .register_background_task(package.package_id, task_id, publisher.principal_id, seed)
        .expect("background task replay");
    assert_eq!(replay_background, background);

    let replay_binding = runtime
        .register_process_binding(package.package_id, process_id, publisher.principal_id, seed)
        .expect("process binding replay");
    assert_eq!(replay_binding, binding);

    let lines = inspect.report_lines();
    assert!(lines.iter().any(|line| line == "background_tasks=1"));
    assert!(lines.iter().any(|line| line == "process_bindings=1"));
}

#[test]
fn register_two_process_bindings_then_inspect_readback() {
    let dir = TempDir::new("application-registrations-two-process");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let seed = 0xF2_u8;
    let publisher = runtime.bootstrap_publisher(seed).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, seed, &fixture_bytes(seed, 48))
        .expect("publish");
    let verification = runtime
        .verify_signed_package(&package, seed)
        .expect("verify");
    runtime
        .install_verified_package(&verification, seed)
        .expect("install");

    let process_id_a = ProcessId::from_bytes([seed.wrapping_add(41); 16]);
    let process_id_b = ProcessId::from_bytes([seed.wrapping_add(42); 16]);

    let binding_a = runtime
        .register_process_binding(
            package.package_id,
            process_id_a,
            publisher.principal_id,
            seed,
        )
        .expect("register first process binding");
    assert_eq!(binding_a.process_id, process_id_a);

    let binding_b = runtime
        .register_process_binding(
            package.package_id,
            process_id_b,
            publisher.principal_id,
            seed.wrapping_add(1),
        )
        .expect("register second process binding");
    assert_eq!(binding_b.process_id, process_id_b);

    let inspect = runtime
        .inspect_application_registrations(package.package_id)
        .expect("inspect registrations");
    assert_eq!(inspect.background_tasks.len(), 0);
    assert_eq!(inspect.process_bindings.len(), 2);

    // ApplicationAuthority orders by `registered_at_ms ASC, idempotency_key ASC`.
    let mut stable_order = inspect.process_bindings.clone();
    stable_order.sort_by(|left, right| {
        left.registered_at_ms
            .cmp(&right.registered_at_ms)
            .then_with(|| {
                left.idempotency_key
                    .as_bytes()
                    .cmp(right.idempotency_key.as_bytes())
            })
    });
    assert_eq!(inspect.process_bindings, stable_order);
    assert_eq!(inspect.process_bindings[0], binding_a);
    assert_eq!(inspect.process_bindings[1], binding_b);

    let replay_a = runtime
        .register_process_binding(
            package.package_id,
            process_id_a,
            publisher.principal_id,
            seed,
        )
        .expect("first process binding replay");
    assert_eq!(replay_a, binding_a);

    let replay_b = runtime
        .register_process_binding(
            package.package_id,
            process_id_b,
            publisher.principal_id,
            seed.wrapping_add(1),
        )
        .expect("second process binding replay");
    assert_eq!(replay_b, binding_b);

    let lines = inspect.report_lines();
    assert!(lines.iter().any(|line| line == "process_bindings=2"));
}

#[test]
fn uninstall_with_registered_background_task_is_fail_closed_via_probe() {
    let dir = TempDir::new("application-registrations-gate");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let seed = 0xF1_u8;
    let publisher = runtime.bootstrap_publisher(seed).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, seed, &fixture_bytes(seed, 48))
        .expect("publish");
    let verification = runtime
        .verify_signed_package(&package, seed)
        .expect("verify");
    runtime
        .install_verified_package(&verification, seed)
        .expect("install");

    let task_id = TaskId::from_bytes([seed.wrapping_add(40); 16]);
    runtime
        .register_background_task(package.package_id, task_id, publisher.principal_id, seed)
        .expect("register background task");

    let inspect = runtime
        .inspect_application_registrations(package.package_id)
        .expect("inspect before gate");
    assert_eq!(inspect.background_tasks.len(), 1);

    let probe = FixedTaskProbe {
        count: u64::try_from(inspect.background_tasks.len()).expect("count fits u64"),
    };
    let uninstalled_at_ms = runtime
        .wall_now_ms(seeded_key(seed, 17))
        .expect("wall for uninstall");
    let error = runtime
        .applications
        .uninstall_application_with_activity_gate(
            UninstallApplicationRequest {
                package_id: package.package_id,
                idempotency_key: seeded_key(seed, 18),
                uninstalled_at_ms,
            },
            &probe,
        )
        .expect_err("registered background task must block fresh uninstall");
    assert!(matches!(
        error,
        ApplicationAuthorityError::ApplicationActiveTasksRunning {
            active_task_count: 1,
            ..
        }
    ));

    let application = runtime
        .applications
        .inspect_application(package.package_id)
        .expect("application still installed")
        .expect("application exists");
    assert_eq!(
        application.status,
        nlos_application::ApplicationStatus::Installed
    );
}
