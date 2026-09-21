//! B-SLICE-K-001 lifecycle tail: uninstall wiring over the landed
//! `ApplicationAuthority::uninstall_application` API.

use nlos_application::{ApplicationAuthorityError, ApplicationStatus, DisableApplicationRequest};
use nlos_artifact::{CollectOrphanBlobsDecision, PackageVerificationReceipt};
use nlos_slice_k::{
    AutoOrphanGc, PublishedPackage, SliceKRuntime, artifact_blob_path, fixture_bytes,
    plant_orphan_artifact_blob, seeded_key,
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

/// Root-cause pin for the registered STEP 09d defect (B-SLICE-K-001 §17,
/// RISK-B-12): a default install attempt runs its install-scoped orphan
/// pass BEFORE the install authority call, so even an attempt the
/// authority refuses (uninstalled application, fail-closed) still
/// collects pre-existing orphan blobs under its own durable receipt.
/// This is designed W22-001 behavior, not a production GC bug.
#[test]
fn refused_reinstall_with_default_gc_still_collects_preexisting_orphans() {
    let (_dir, runtime, package, verification) = installed_fixture(0xF5);

    let (orphan, orphan_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xA3, 64).expect("plant orphan");
    runtime
        .uninstall_application(package.package_id, 0xF5)
        .expect("uninstall");

    assert!(
        runtime
            .install_verified_package_by_id(verification.receipt_id, 0xF7)
            .is_err(),
        "reinstall over an uninstalled application must fail closed"
    );
    assert!(
        !orphan_path.exists(),
        "the install-scoped pass ran before the refusal and collected the orphan"
    );

    let readback = runtime.install_orphan_gc(0xF7).expect("gc readback");
    assert!(matches!(readback, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(readback.receipt().collected_digests, vec![orphan]);
}

/// Fix pin for the same defect: the demo's fail-closed refusal probe opts
/// out of the install-scoped pass, so the STEP 09d manual pass collects
/// exactly the planted orphans and referenced blobs survive.
#[test]
fn refused_reinstall_with_gc_disabled_keeps_planted_orphans_for_manual_gc() {
    let (_dir, runtime, package, verification) = installed_fixture(0xF0);

    let (orphan_a, orphan_a_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xA1, 128).expect("plant orphan A");
    let (orphan_b, orphan_b_path) =
        plant_orphan_artifact_blob(runtime.root(), 0xA2, 96).expect("plant orphan B");
    runtime
        .uninstall_application(package.package_id, 0xF0)
        .expect("uninstall");

    assert!(
        runtime
            .install_verified_package_by_id_with_gc(
                verification.receipt_id,
                0xF2,
                AutoOrphanGc::Disabled
            )
            .is_err(),
        "reinstall over an uninstalled application must fail closed"
    );
    assert!(
        orphan_a_path.is_file() && orphan_b_path.is_file(),
        "the opt-out must leave the planted orphans for the manual pass"
    );

    let gc = runtime
        .collect_orphan_blobs(0xF0)
        .expect("manual orphan GC");
    assert!(matches!(gc, CollectOrphanBlobsDecision::Collected(_)));
    let mut expected = vec![orphan_a, orphan_b];
    expected.sort();
    assert_eq!(gc.receipt().collected_digests, expected);
    assert!(!orphan_a_path.exists());
    assert!(!orphan_b_path.exists());
    assert!(
        artifact_blob_path(runtime.root(), package.payload_digest).is_file(),
        "referenced package payload blob must survive GC"
    );

    let install_scoped = runtime.install_orphan_gc(0xF2).expect("gc readback");
    assert!(
        matches!(install_scoped, CollectOrphanBlobsDecision::Collected(_)),
        "the Disabled refusal must not have consumed the install-scoped key"
    );
    assert!(install_scoped.receipt().collected_digests.is_empty());
}
