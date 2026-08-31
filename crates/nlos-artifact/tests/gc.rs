//! B-ARTIFACT minimal GC: explicit conservative orphan-blob collection,
//! receipt durability/replay, and crash-window consistency.
//!
//! The fault rows reuse the `nlos-store-fault` VFS shim exactly like
//! `fault_injection.rs`: the shim intercepts only `SQLite` I/O, while GC
//! blob removals are ordinary filesystem I/O. That asymmetry is the point
//! of the matrix — both rows pin the pre-receipt-commit window (removals
//! durable, receipt absent, state consistent).

use std::fs;
use std::sync::{Mutex, MutexGuard};

use nlos_artifact::{
    ArtifactError, ArtifactStore, CollectOrphanBlobsDecision, CollectOrphanBlobsRequest,
    ContentDigest, StageRevisionRequest,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskId};
use support::{TestStoreDir, artifact_id, artifact_spec, bytes, put};

mod support;

const VFS_NAME: &str = "nlos-artifact-gc-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn open_shim(root: &std::path::Path) -> ArtifactStore {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    ArtifactStore::open_with_vfs(root, Some(VFS_NAME)).expect("open via fault vfs")
}

fn gc_request(key: u8) -> CollectOrphanBlobsRequest {
    CollectOrphanBlobsRequest {
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        collected_at_ms: 9_000,
    }
}

/// Plants a fully-formed blob file with no metadata row (the
/// "after rename, before metadata commit" crash residue).
fn plant_orphan(directory: &TestStoreDir, tag: u8, len: usize) -> ContentDigest {
    let payload = bytes(tag, len);
    let digest = ContentDigest::of_bytes(&payload);
    let path = directory.artifact_blob(digest);
    fs::create_dir_all(path.parent().expect("shard dir")).expect("shard dir");
    fs::write(&path, &payload).expect("plant orphan blob");
    digest
}

fn stage(store: &ArtifactStore, artifact: ArtifactId, expected_head: u64, payload: &[u8], key: u8) {
    store
        .stage_revision(StageRevisionRequest {
            artifact_id: artifact,
            expected_head_revision: expected_head,
            bytes: payload,
            task_id: TaskId::from_bytes([0x77; 16]),
            permit_id: CommitPermitId::from_bytes([0x78; 16]),
            write_set_root: ContentDigest::of_bytes(b"write-set"),
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            created_at_ms: 6_000,
        })
        .expect("stage");
}

fn assert_integrity(root: &std::path::Path) {
    let connection =
        rusqlite::Connection::open(root.join("metadata.db")).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

/// Normal GC: provable orphans are removed, every in-registry reference
/// (two committed revisions, a second artifact's revision, an unreleased
/// staged blob, a live head) is retained, the cache retention domain and
/// foreign files are untouched.
#[test]
fn gc_collects_provable_orphans_and_retains_every_referenced_blob() {
    let directory = TestStoreDir::new("gc-normal");
    let store = ArtifactStore::open(directory.root()).expect("open");

    let p1 = bytes(0x51, 256);
    let p2 = bytes(0x52, 256);
    let p3 = bytes(0x53, 256);
    let p4 = bytes(0x54, 256);
    store.create_artifact(artifact_spec(0x30)).expect("create");
    store
        .put_revision(put(artifact_id(0x30), 0, &p1))
        .expect("put revision 1");
    store
        .put_revision(put(artifact_id(0x30), 1, &p2))
        .expect("put revision 2");
    store.create_artifact(artifact_spec(0x31)).expect("create");
    store
        .put_revision(put(artifact_id(0x31), 0, &p3))
        .expect("put artifact B");
    stage(&store, artifact_id(0x30), 2, &p4, 0x79);

    let orphan_a = plant_orphan(&directory, 0x61, 128);
    // Identical bytes in BOTH domains: the artifacts copy is an orphan,
    // the cache copy is a live cache entry. GC must remove only the
    // artifacts-domain file.
    let shared = bytes(0x62, 128);
    let shared_digest = ContentDigest::of_bytes(&shared);
    let orphan_b_path = directory.artifact_blob(shared_digest);
    fs::create_dir_all(orphan_b_path.parent().expect("shard dir")).expect("shard dir");
    fs::write(&orphan_b_path, &shared).expect("plant cross-domain orphan");
    store
        .put_cache_blob("gc/shared", &shared, 7_000)
        .expect("cache put");

    // Foreign junk is reported by recover, never a GC candidate.
    let foreign_dir = directory.root().join("artifacts/blobs/zz");
    fs::create_dir_all(&foreign_dir).expect("foreign shard");
    fs::write(foreign_dir.join("not-a-digest.bin"), b"junk").expect("foreign file");

    let decision = store
        .collect_orphan_blobs(gc_request(0x01))
        .expect("first gc");
    assert!(matches!(decision, CollectOrphanBlobsDecision::Collected(_)));
    let receipt = decision.receipt();
    let mut expected = vec![orphan_a, shared_digest];
    expected.sort();
    assert_eq!(receipt.collected_digests, expected);
    assert_eq!(receipt.collected_count, 2);
    assert_eq!(receipt.scanned_blob_count, 6, "p1..p4 + two orphans");
    assert_eq!(receipt.created_at_ms, 9_000);

    // Removed exactly the sentenced artifacts-domain files.
    assert!(!directory.artifact_blob(orphan_a).exists());
    assert!(!orphan_b_path.exists());
    // Negative path: every referenced blob survives on disk...
    for payload in [&p1, &p2, &p3, &p4] {
        assert!(
            directory
                .artifact_blob(ContentDigest::of_bytes(payload))
                .is_file(),
            "referenced blob must survive GC"
        );
    }
    // ...including the unreleased staged blob (p4), and the cache-domain
    // copy of the shared digest.
    assert!(directory.cache_blob(shared_digest).is_file());
    assert_eq!(
        store.get_cache_blob("gc/shared").expect("cache get"),
        Some(shared)
    );
    // Foreign junk survives.
    assert!(foreign_dir.join("not-a-digest.bin").is_file());

    // Active reads unaffected.
    assert_eq!(store.get_revision(artifact_id(0x30), 1).expect("r1"), p1);
    assert_eq!(store.get_revision(artifact_id(0x30), 2).expect("r2"), p2);
    assert_eq!(store.get_revision(artifact_id(0x31), 1).expect("b1"), p3);

    let report = store.recover().expect("recover");
    assert!(report.orphan_blobs.is_empty());
    assert!(report.missing_blobs.is_empty());
    assert!(report.missing_staged_blobs.is_empty());

    let readback = store
        .inspect_gc_receipt(receipt.receipt_id)
        .expect("inspect receipt");
    assert_eq!(readback, *receipt);
}

/// Replay is durable-authoritative: the same idempotency key returns the
/// stored receipt verbatim (across restarts too) and never re-runs, so a
/// new orphan planted after the run survives until an explicitly new run.
#[test]
fn gc_replay_is_durable_authoritative_and_never_reruns() {
    let directory = TestStoreDir::new("gc-replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x32)).expect("create");
    let p1 = bytes(0x55, 128);
    store
        .put_revision(put(artifact_id(0x32), 0, &p1))
        .expect("put");

    let orphan1 = plant_orphan(&directory, 0x63, 64);
    let first = store.collect_orphan_blobs(gc_request(0x0a)).expect("gc");
    assert!(matches!(first, CollectOrphanBlobsDecision::Collected(_)));
    assert_eq!(first.receipt().collected_digests, vec![orphan1]);
    assert!(!directory.artifact_blob(orphan1).exists());

    // A new orphan appears after the completed run.
    let orphan2 = plant_orphan(&directory, 0x64, 64);

    let replay = store
        .collect_orphan_blobs(gc_request(0x0a))
        .expect("replay");
    assert!(matches!(replay, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(replay.receipt(), first.receipt());
    assert!(
        directory.artifact_blob(orphan2).is_file(),
        "replay must not re-run the collection"
    );

    // A fresh key runs a fresh scan.
    let second = store.collect_orphan_blobs(gc_request(0x0b)).expect("gc 2");
    assert!(matches!(second, CollectOrphanBlobsDecision::Collected(_)));
    assert_eq!(second.receipt().collected_digests, vec![orphan2]);
    assert!(!directory.artifact_blob(orphan2).exists());

    // Replay is durable across restart.
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    let after = reopened
        .collect_orphan_blobs(gc_request(0x0a))
        .expect("replay after reopen");
    assert!(matches!(after, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(after.receipt(), first.receipt());
    assert_eq!(
        reopened
            .inspect_gc_receipt(first.receipt().receipt_id)
            .expect("inspect after reopen"),
        *first.receipt()
    );
}

/// Row 1 of the kill window (pre-commit IOERR): the receipt insert fails
/// under `FailWritesAfter { 0, IoErr }` after the file removals already
/// happened (plain fs I/O, not VFS-mediated). Reopen: no phantom receipt,
/// orphans gone, in-registry state intact; the retry recomputes the now
/// empty diff and completes the receipt; replay is then exact.
#[test]
fn gc_io_error_during_receipt_commit_leaves_consistent_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("gc-ioerr");
    let store = open_shim(directory.root());
    store.create_artifact(artifact_spec(0x33)).expect("create");
    let p1 = bytes(0x56, 128);
    store
        .put_revision(put(artifact_id(0x33), 0, &p1))
        .expect("put");

    let orphan1 = plant_orphan(&directory, 0x65, 64);
    let orphan2 = plant_orphan(&directory, 0x66, 64);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = store
        .collect_orphan_blobs(gc_request(0x0c))
        .expect_err("receipt commit must fail under injected I/O error");
    nlos_store_fault::disarm();
    assert!(
        matches!(error, ArtifactError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    assert!(nlos_store_fault::writes_observed() > 0);

    // The removals preceded the failed receipt write and are durable.
    assert!(!directory.artifact_blob(orphan1).exists());
    assert!(!directory.artifact_blob(orphan2).exists());
    // In-registry state is intact and typed reads still work.
    assert_eq!(
        store.get_revision(artifact_id(0x33), 1).expect("revision"),
        p1
    );

    // Reopen: consistent — orphans gone, nothing missing, no receipt row.
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    let report = reopened.recover().expect("recover");
    assert!(report.orphan_blobs.is_empty());
    assert!(report.missing_blobs.is_empty());
    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw open");
    let count: i64 = raw
        .query_row("SELECT COUNT(*) FROM artifact_gc_receipts", [], |row| {
            row.get(0)
        })
        .expect("count receipts");
    assert_eq!(count, 0, "no phantom receipt may exist");
    drop(raw);

    // The retry recomputes the (now empty) diff and completes the receipt;
    // further replays of the same key are exact.
    let retry = reopened
        .collect_orphan_blobs(gc_request(0x0c))
        .expect("retry gc");
    assert!(matches!(retry, CollectOrphanBlobsDecision::Collected(_)));
    assert!(retry.receipt().collected_digests.is_empty());
    assert_eq!(retry.receipt().scanned_blob_count, 1, "only p1 remains");
    let again = reopened
        .collect_orphan_blobs(gc_request(0x0c))
        .expect("replay");
    assert!(matches!(again, CollectOrphanBlobsDecision::Replayed(_)));
    assert_eq!(again.receipt(), retry.receipt());
    assert_integrity(directory.root());
}

/// Row 2 of the kill window (pre-commit power loss): the receipt commit is
/// silently dropped, so GC "reports success" but the phantom receipt is
/// invisible after reopen (the wal-index-holding connection dies first).
/// Removals were real fs I/O and survive; the rerun completes idempotently.
#[test]
fn gc_power_loss_mid_commit_phantom_receipt_invisible_after_reopen() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let directory = TestStoreDir::new("gc-power-loss");
    let store = open_shim(directory.root());
    store.create_artifact(artifact_spec(0x34)).expect("create");
    let p1 = bytes(0x57, 128);
    store
        .put_revision(put(artifact_id(0x34), 0, &p1))
        .expect("put");

    let orphan1 = plant_orphan(&directory, 0x67, 64);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = store
        .collect_orphan_blobs(gc_request(0x0d))
        .expect("gc under power loss reports success");
    assert!(matches!(phantom, CollectOrphanBlobsDecision::Collected(_)));
    nlos_store_fault::disarm();

    // The surviving connection's wal-index references frames the disk
    // never saw; it must die first (as a real power loss would kill it).
    drop(store);

    let recovered = ArtifactStore::open(directory.root()).expect("reopen");
    assert!(matches!(
        recovered.inspect_gc_receipt(phantom.receipt().receipt_id),
        Err(ArtifactError::GcReceiptNotFound(_))
    ));
    // Removals were plain fs I/O and survived the simulated power loss,
    // while the referenced blob is intact: a consistent, receipt-less
    // post-crash state.
    assert!(!directory.artifact_blob(orphan1).exists());
    assert_eq!(
        recovered
            .get_revision(artifact_id(0x34), 1)
            .expect("revision"),
        p1
    );
    let report = recovered.recover().expect("recover");
    assert!(report.orphan_blobs.is_empty());
    assert!(report.missing_blobs.is_empty());

    let completion = recovered
        .collect_orphan_blobs(gc_request(0x0d))
        .expect("complete gc");
    assert!(matches!(
        completion,
        CollectOrphanBlobsDecision::Collected(_)
    ));
    assert!(completion.receipt().collected_digests.is_empty());
    assert_integrity(directory.root());
}
