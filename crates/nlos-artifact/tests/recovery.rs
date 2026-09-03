//! B-ARTIFACT-001 recovery/reconcile and cache-domain separation
//! (`[CTX-NOTDATA-001]` spirit).

mod support;

use std::fs;

use nlos_artifact::{ArtifactError, ArtifactStore, ContentDigest};
use support::{READ_NOW_MS, TestStoreDir, artifact_id, artifact_spec, bytes, put};

#[test]
fn missing_blob_is_reported_and_get_fails_typed_with_hint() {
    let directory = TestStoreDir::new("missing-blob");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x20)).expect("create");
    let payload = bytes(0x71, 768);
    let digest = ContentDigest::of_bytes(&payload);
    store
        .put_revision(put(artifact_id(0x20), 0, &payload))
        .expect("put");

    // Operator/介质事故: the blob file vanishes behind the store's back.
    fs::remove_file(directory.artifact_blob(digest)).expect("remove blob");

    let error = store
        .get_revision(artifact_id(0x20), 1, READ_NOW_MS)
        .expect_err("missing blob must fail typed");
    match &error {
        ArtifactError::BlobMissing {
            artifact_id: id,
            revision,
            digest: reported,
            ..
        } => {
            assert_eq!(*id, artifact_id(0x20));
            assert_eq!(*revision, 1);
            assert_eq!(*reported, digest);
        }
        other => panic!("expected BlobMissing, got {other}"),
    }
    assert!(
        error.to_string().contains(&digest.to_hex()),
        "error must name the digest for operators: {error}"
    );

    let report = store.recover().expect("recover");
    assert_eq!(report.missing_blobs.len(), 1);
    assert_eq!(report.missing_blobs[0].digest, digest);
    assert_eq!(report.missing_blobs[0].revision, 1);
    assert!(report.orphan_blobs.is_empty());
    // recover reconciles but never fabricates bytes.
    assert!(matches!(
        store.get_revision(artifact_id(0x20), 1, READ_NOW_MS),
        Err(ArtifactError::BlobMissing { .. })
    ));
}

#[test]
fn torn_blob_fails_digest_verification_never_returns_wrong_bytes() {
    let directory = TestStoreDir::new("torn-blob");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x21)).expect("create");
    let payload = bytes(0x81, 2048);
    let digest = ContentDigest::of_bytes(&payload);
    store
        .put_revision(put(artifact_id(0x21), 0, &payload))
        .expect("put");

    // Torn write: the blob file is truncated after commit.
    let blob_path = directory.artifact_blob(digest);
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&blob_path)
        .expect("open blob");
    file.set_len(1024).expect("truncate blob");
    drop(file);

    let error = store
        .get_revision(artifact_id(0x21), 1, READ_NOW_MS)
        .expect_err("torn blob must fail verification");
    match &error {
        ArtifactError::DigestMismatch {
            expected, actual, ..
        } => {
            assert_eq!(*expected, digest);
            assert_eq!(*actual, ContentDigest::of_bytes(&payload[..1024]));
        }
        other => panic!("expected DigestMismatch, got {other}"),
    }
}

#[test]
fn orphan_tmp_files_are_cleaned_and_orphan_blobs_only_listed() {
    let directory = TestStoreDir::new("orphans");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x22)).expect("create");
    let payload = bytes(0x91, 256);
    store
        .put_revision(put(artifact_id(0x22), 0, &payload))
        .expect("put");

    // Crash before rename: a pre-rename tmp file is left behind.
    let tmp_path = directory.root().join("artifacts/tmp/deadbeef.0.tmp");
    fs::write(&tmp_path, b"partial write").expect("plant tmp");

    // Crash after rename but before metadata commit: a fully committed
    // blob no revision references.
    let orphan_payload = bytes(0x92, 256);
    let orphan_digest = ContentDigest::of_bytes(&orphan_payload);
    let orphan_path = directory.artifact_blob(orphan_digest);
    fs::create_dir_all(orphan_path.parent().expect("shard")).expect("shard dir");
    fs::write(&orphan_path, &orphan_payload).expect("plant orphan blob");

    let report = store.recover().expect("recover");
    assert_eq!(report.removed_tmp_files, 1);
    assert!(!tmp_path.exists(), "tmp orphan must be removed");
    assert_eq!(report.orphan_blobs, vec![orphan_digest]);
    assert!(
        orphan_path.is_file(),
        "orphan blobs are listed for GC, never deleted in this slice"
    );
    assert!(report.missing_blobs.is_empty());

    // Recovery is idempotent: a second run on the reconciled state only
    // re-reports the deliberately-retained orphan blob.
    let second = store.recover().expect("second recover");
    assert_eq!(second.removed_tmp_files, 0);
    assert_eq!(second.orphan_blobs, vec![orphan_digest]);

    // The committed revision is unaffected.
    assert_eq!(
        store
            .get_revision(artifact_id(0x22), 1, READ_NOW_MS)
            .expect("revision 1"),
        payload
    );
}

#[test]
fn cache_eviction_never_touches_artifact_blobs() {
    let directory = TestStoreDir::new("cache-separation");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x23)).expect("create");

    // Deliberately identical content in both domains: the digest collides
    // across domains, so a confused implementation would corrupt artifacts.
    let shared = bytes(0xa1, 512);
    let digest = ContentDigest::of_bytes(&shared);
    store
        .put_revision(put(artifact_id(0x23), 0, &shared))
        .expect("put artifact revision");
    let cache_digest = store
        .put_cache_blob("ctx/build/1", &shared, 7_000)
        .expect("put cache");
    assert_eq!(cache_digest, digest);
    assert!(directory.artifact_blob(digest).is_file());
    assert!(directory.cache_blob(digest).is_file());
    assert_eq!(
        store.get_cache_blob("ctx/build/1").expect("cache get"),
        Some(shared.clone())
    );

    // Cache eviction removes only the cache copy.
    assert!(store.evict_cache_blob("ctx/build/1").expect("evict"));
    assert!(!directory.cache_blob(digest).exists());
    assert_eq!(store.get_cache_blob("ctx/build/1").expect("miss"), None);
    assert!(
        directory.artifact_blob(digest).is_file(),
        "artifact blob must survive cache eviction"
    );
    assert_eq!(
        store
            .get_revision(artifact_id(0x23), 1, READ_NOW_MS)
            .expect("artifact revision"),
        shared
    );
    let report = store.recover().expect("recover");
    assert!(report.missing_blobs.is_empty());
    assert!(report.orphan_blobs.is_empty());

    // Evicting an unknown key is a no-op.
    assert!(
        !store
            .evict_cache_blob("ctx/build/unknown")
            .expect("evict miss")
    );
}

#[test]
fn cache_blob_loss_degrades_to_miss_and_recover_drops_row() {
    let directory = TestStoreDir::new("cache-loss");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x24)).expect("create");
    store
        .put_revision(put(artifact_id(0x24), 0, &bytes(0xb1, 128)))
        .expect("put revision");

    let cached = bytes(0xb2, 128);
    let cache_digest = store
        .put_cache_blob("emb/doc/9", &cached, 7_000)
        .expect("cache put");
    fs::remove_file(directory.cache_blob(cache_digest)).expect("lose cache blob");

    // Best-effort semantics: a vanished cache blob is a miss, not an error.
    assert_eq!(store.get_cache_blob("emb/doc/9").expect("miss"), None);

    // Plant an orphan cache blob (no row references it).
    let orphan = ContentDigest::of_bytes(&bytes(0xb3, 64));
    let orphan_path = directory.cache_blob(orphan);
    fs::create_dir_all(orphan_path.parent().expect("shard")).expect("shard");
    fs::write(&orphan_path, bytes(0xb3, 64)).expect("plant orphan cache blob");

    let report = store.recover().expect("recover");
    assert_eq!(report.cache_rows_dropped, 1);
    assert_eq!(report.orphan_cache_blobs, vec![orphan]);
    assert!(report.missing_blobs.is_empty());
    assert!(report.orphan_blobs.is_empty());

    // After the row drop, the key behaves as a plain miss.
    assert_eq!(store.get_cache_blob("emb/doc/9").expect("miss"), None);
}
