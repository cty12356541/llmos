//! B-SLICE-K-001 lifecycle tail: uninstall wiring over the landed
//! `ApplicationAuthority::uninstall_application` API.

use nlos_application::{ApplicationAuthorityError, ApplicationStatus, DisableApplicationRequest};
use nlos_artifact::{CollectOrphanBlobsDecision, PackageVerificationReceipt};
use nlos_slice_k::{
    PublishedPackage, SliceKRuntime, artifact_blob_path, fixture_bytes, plant_orphan_artifact_blob,
    seeded_key,
};

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

fn installed_fixture(
    seed: u8,
) -> (
    TempDir,
    SliceKRuntime,
    PublishedPackage,
    PackageVerificationReceipt,
) {
    let dir = TempDir::new("lifecycle-uninstall");
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(seed).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, seed, &fixture_bytes(seed, 64))
        .expect("publish");
    let verification = runtime
        .verify_signed_package(&package, seed)
        .expect("verify");
    runtime
        .install_verified_package(&verification, seed)
        .expect("install");
    (dir, runtime, package, verification)
}

#[test]
fn install_then_uninstall_reaches_terminal_state_and_refuses_reinstall() {
    let (_dir, runtime, package, verification) = installed_fixture(0xD0);

    let uninstall = runtime
        .uninstall_application(package.package_id, 0xD0)
        .expect("uninstall from installed");

    let application = runtime
        .applications
        .inspect_application(package.package_id)
        .expect("application readback")
        .expect("application exists");
    assert_eq!(application.status, ApplicationStatus::Uninstalled);
    assert_eq!(
        application.current_installation_generation,
        uninstall.application_generation
    );

    let receipt = runtime
        .applications
        .inspect_uninstall_receipt(package.package_id)
        .expect("uninstall receipt readback")
        .expect("uninstall receipt exists");
    assert_eq!(receipt, uninstall);

    assert!(
        runtime
            .install_verified_package(&verification, 0xD1)
            .is_err(),
        "reinstall over an uninstalled application must fail closed"
    );

    let replay = runtime
        .uninstall_application(package.package_id, 0xD0)
        .expect("uninstall replay");
    assert_eq!(replay, uninstall);
}

#[test]
fn install_disable_then_uninstall_reaches_terminal_state_and_refuses_reinstall() {
    let (_dir, runtime, package, verification) = installed_fixture(0xD2);

    let advanced = runtime
        .install_verified_package_by_id(verification.receipt_id, 0xD3)
        .expect("reinstall advances generation");
    assert!(advanced.installation_generation.get() >= 2);

    let disabled_at_ms = runtime
        .wall_now_ms(seeded_key(0xD2, 90))
        .expect("wall for disable");
    runtime
        .applications
        .disable_application(DisableApplicationRequest {
            package_id: package.package_id,
            idempotency_key: seeded_key(0xD2, 91),
            disabled_at_ms,
        })
        .expect("disable");

    let disabled = runtime
        .applications
        .inspect_application(package.package_id)
        .expect("readback")
        .expect("application");
    assert_eq!(disabled.status, ApplicationStatus::Disabled);

    let uninstall = runtime
        .uninstall_application(package.package_id, 0xD2)
        .expect("uninstall from disabled");
    assert_eq!(
        uninstall.application_generation,
        disabled.current_installation_generation
    );

    let terminal = runtime
        .applications
        .inspect_application(package.package_id)
        .expect("readback")
        .expect("application");
    assert_eq!(terminal.status, ApplicationStatus::Uninstalled);

    let error = runtime
        .install_verified_package_by_id(verification.receipt_id, 0xD4)
        .expect_err("reinstall after uninstall must fail closed");
    assert!(matches!(
        error,
        nlos_slice_k::SliceKError::Application(
            ApplicationAuthorityError::ApplicationUninstalled { .. }
        )
    ));
}

#[test]
fn uninstall_then_manual_gc_collects_package_orphans_and_retains_referenced_blobs() {
    let (_dir, runtime, package, _verification) = installed_fixture(0xE0);

    let (orphan_a, orphan_a_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xED, 128).expect("plant orphan A");
    let (orphan_b, orphan_b_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xEE, 64).expect("plant orphan B");

    runtime
        .uninstall_application(package.package_id, 0xE0)
        .expect("uninstall");

    let gc = runtime
        .collect_orphan_blobs(0xE0)
        .expect("manual orphan GC");
    assert!(matches!(gc, CollectOrphanBlobsDecision::Collected(_)));
    let receipt = gc.receipt();
    let mut expected = vec![orphan_a, orphan_b];
    expected.sort();
    assert_eq!(receipt.collected_digests, expected);
    assert_eq!(receipt.collected_count, 2);
    assert!(!orphan_a_path.exists());
    assert!(!orphan_b_path.exists());

    assert!(
        artifact_blob_path(runtime.root(), package.payload_digest).is_file(),
        "referenced package payload blob must survive GC"
    );

    let replay = runtime.collect_orphan_blobs(0xE0).expect("GC replay");
    assert!(matches!(replay, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(replay.receipt(), receipt);
}
