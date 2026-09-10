//! W22-001 ROAD-B-001: install-time automatic orphan-blob GC prefix.
//!
//! The install path runs the install-scoped GC pass
//! ([`SliceKRuntime::install_orphan_gc`], keys `seeded_key(seed, 21/22)`)
//! before the install authority call by default; `AutoOrphanGc::Disabled`
//! bypasses it. The manual pass (`collect_orphan_blobs`, keys 19/20) never
//! aliases the install-scoped receipts.

use nlos_artifact::{CollectOrphanBlobsDecision, PackageVerificationReceipt};
use nlos_slice_k::{
    AutoOrphanGc, PublishedPackage, SliceKRuntime, artifact_blob_path, fixture_bytes,
    plant_orphan_artifact_blob,
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

fn verified_fixture(
    name: &str,
    seed: u8,
) -> (
    TempDir,
    SliceKRuntime,
    PublishedPackage,
    PackageVerificationReceipt,
) {
    let dir = TempDir::new(name);
    let runtime = SliceKRuntime::open(dir.root()).expect("open slice-k runtime");
    let publisher = runtime.bootstrap_publisher(seed).expect("publisher");
    let package = runtime
        .publish_signed_package(&publisher, seed, &fixture_bytes(seed, 64))
        .expect("publish");
    let verification = runtime
        .verify_signed_package(&package, seed)
        .expect("verify");
    (dir, runtime, package, verification)
}

#[test]
fn install_auto_orphan_gc_by_default_collects_preexisting_orphans_and_replays_receipt() {
    let (_dir, runtime, package, verification) = verified_fixture("install-gc-auto", 0x5A);

    let (orphan_digest, orphan_path) =
        plant_orphan_artifact_blob(runtime.root(), 0x5D, 128).expect("plant orphan");

    let install = runtime
        .install_verified_package(&verification, 0x5A)
        .expect("install (default auto orphan gc)");
    assert_eq!(install.package_id, package.package_id);
    assert!(
        !orphan_path.exists(),
        "pre-existing orphan must be collected by the default install-time pass"
    );
    assert!(
        artifact_blob_path(runtime.root(), package.payload_digest).is_file(),
        "referenced package payload blob must survive the install-time pass"
    );

    let replay = runtime.install_orphan_gc(0x5A).expect("gc readback");
    assert!(
        matches!(replay, CollectOrphanBlobsDecision::Replayed(_)),
        "the install-time receipt must be durably recorded and replayed"
    );
    assert_eq!(replay.receipt().collected_digests, vec![orphan_digest]);
    assert_eq!(replay.receipt().collected_count, 1);

    let reinstall = runtime
        .install_verified_package_with_gc(&verification, 0x5A, AutoOrphanGc::Enabled)
        .expect("install replay under auto gc");
    assert_eq!(reinstall, install);
}

#[test]
fn install_with_orphan_gc_disabled_keeps_preexisting_orphans_and_pass_still_collects_on_demand() {
    let (_dir, runtime, package, verification) = verified_fixture("install-gc-off", 0x5B);

    let (orphan_digest, orphan_path) =
        plant_orphan_artifact_blob(runtime.root(), 0x5E, 64).expect("plant orphan");

    let install = runtime
        .install_verified_package_with_gc(&verification, 0x5B, AutoOrphanGc::Disabled)
        .expect("install (orphan gc opt-out)");
    assert_eq!(install.package_id, package.package_id);
    assert!(
        orphan_path.is_file(),
        "opt-out must leave the pre-existing orphan on disk"
    );
    assert!(
        artifact_blob_path(runtime.root(), package.payload_digest).is_file(),
        "the install itself must keep the referenced payload blob"
    );

    let on_demand = runtime.install_orphan_gc(0x5B).expect("gc on demand");
    assert!(
        matches!(on_demand, CollectOrphanBlobsDecision::Collected(_)),
        "the opt-out must not have consumed the install-scoped gc key"
    );
    assert_eq!(on_demand.receipt().collected_digests, vec![orphan_digest]);
    assert!(!orphan_path.exists());

    let replay = runtime.install_orphan_gc(0x5B).expect("gc replay");
    assert!(matches!(replay, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(replay.receipt(), on_demand.receipt());
}
