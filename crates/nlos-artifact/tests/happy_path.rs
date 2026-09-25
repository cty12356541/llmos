//! B-ARTIFACT-001 happy path: create -> put -> resolve -> get with digest
//! verification, idempotent creation, durability pragmas, and persistence
//! across reopen.

mod support;

use nlos_artifact::{ArtifactError, ArtifactStore, CreateArtifactDecision, PutRevisionDecision};
use support::{READ_NOW_MS, TestStoreDir, artifact_id, artifact_spec, bytes, put};

#[test]
fn create_put_resolve_get_roundtrip_with_digest_verification() {
    let directory = TestStoreDir::new("roundtrip");
    let store = ArtifactStore::open(directory.root()).expect("open store");

    let record = match store
        .create_artifact(artifact_spec(0x01))
        .expect("create artifact")
    {
        CreateArtifactDecision::Created(record) => record,
        CreateArtifactDecision::Existing(_) => panic!("fresh store must create"),
    };
    assert_eq!(record.artifact_id, artifact_id(0x01));
    assert_eq!(record.head_revision, 0);
    assert_eq!(record.head_digest, None);
    assert_eq!(
        store
            .resolve_head(artifact_id(0x01), READ_NOW_MS)
            .expect("head"),
        None
    );

    let payload_v1 = bytes(0x11, 1024);
    let digest_v1 = nlos_artifact::ContentDigest::of_bytes(&payload_v1);
    let revision_v1 = match store
        .put_revision(put(artifact_id(0x01), 0, &payload_v1))
        .expect("put revision 1")
    {
        PutRevisionDecision::Committed(record) => record,
        PutRevisionDecision::Replayed(_) => panic!("first put must commit"),
    };
    assert_eq!(revision_v1.revision, 1);
    assert_eq!(revision_v1.digest, digest_v1);
    assert_eq!(revision_v1.size_bytes, 1024);

    // The blob is durable at its content address before the metadata commit.
    assert!(directory.artifact_blob(digest_v1).is_file());

    let head = store
        .resolve_head(artifact_id(0x01), READ_NOW_MS)
        .expect("resolve head")
        .expect("head exists after first put");
    assert_eq!(head.revision, 1);
    assert_eq!(head.digest, digest_v1);

    // Reads re-verify the digest against the content address.
    let fetched = store
        .get_revision(artifact_id(0x01), 1, READ_NOW_MS)
        .expect("get revision 1");
    assert_eq!(fetched, payload_v1);

    let payload_v2 = bytes(0x22, 4096);
    let digest_v2 = nlos_artifact::ContentDigest::of_bytes(&payload_v2);
    let revision_v2 = match store
        .put_revision(put(artifact_id(0x01), 1, &payload_v2))
        .expect("put revision 2")
    {
        PutRevisionDecision::Committed(record) => record,
        PutRevisionDecision::Replayed(_) => panic!("second put must commit"),
    };
    assert_eq!(revision_v2.revision, 2);
    assert_eq!(
        store
            .resolve_head(artifact_id(0x01), READ_NOW_MS)
            .expect("head")
            .expect("head"),
        nlos_artifact::HeadState {
            revision: 2,
            digest: digest_v2,
        }
    );

    let revisions = store.list_revisions(artifact_id(0x01)).expect("list");
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].digest, digest_v1);
    assert_eq!(revisions[1].digest, digest_v2);
    let inspected = store
        .inspect_revision(artifact_id(0x01), 1)
        .expect("inspect revision");
    assert_eq!(inspected, revision_v1);
    let inspected_artifact = store
        .inspect_artifact(artifact_id(0x01))
        .expect("inspect artifact");
    assert_eq!(inspected_artifact.head_revision, 2);
    assert_eq!(inspected_artifact.head_digest, Some(digest_v2));

    // Everything survives a reopen (metadata + bytes).
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    assert_eq!(
        reopened
            .resolve_head(artifact_id(0x01), READ_NOW_MS)
            .expect("head after reopen")
            .expect("head"),
        nlos_artifact::HeadState {
            revision: 2,
            digest: digest_v2,
        }
    );
    assert_eq!(
        reopened
            .get_revision(artifact_id(0x01), 1, READ_NOW_MS)
            .expect("old revision after reopen"),
        payload_v1
    );
    assert_eq!(
        reopened
            .get_revision(artifact_id(0x01), 2, READ_NOW_MS)
            .expect("head revision after reopen"),
        payload_v2
    );
}

#[test]
fn create_artifact_is_idempotent_by_caller_key() {
    let directory = TestStoreDir::new("idempotent");
    let store = ArtifactStore::open(directory.root()).expect("open");

    let spec = artifact_spec(0x02);
    let created = store.create_artifact(spec.clone()).expect("create");
    let created_record = created.record().clone();

    // Exact replay under the same key returns the stored record.
    let replayed = store.create_artifact(spec).expect("replay create");
    match replayed {
        CreateArtifactDecision::Existing(record) => assert_eq!(record, created_record),
        CreateArtifactDecision::Created(_) => panic!("replay must return Existing"),
    }

    // Same key, different specification -> typed conflict.
    let mut conflicting = artifact_spec(0x02);
    conflicting.content_type = "text/plain".to_string();
    let error = store
        .create_artifact(conflicting)
        .expect_err("key reuse with different spec must fail");
    assert!(matches!(error, ArtifactError::IdempotencyConflict));

    // Same artifact identity, different key -> typed conflict.
    let mut different_key = artifact_spec(0x02);
    different_key.idempotency_key = nlos_types::IdempotencyKey::from_bytes([0xff; 16]);
    let error = store
        .create_artifact(different_key)
        .expect_err("artifact identity reuse must fail");
    assert!(matches!(error, ArtifactError::IdempotencyConflict));
}

#[test]
fn unknown_artifact_and_revision_are_typed_errors() {
    let directory = TestStoreDir::new("notfound");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x03)).expect("create");

    assert!(matches!(
        store.resolve_head(artifact_id(0x7f), READ_NOW_MS),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
    assert!(matches!(
        store.get_revision(artifact_id(0x7f), 1, READ_NOW_MS),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
    assert!(matches!(
        store.get_revision(artifact_id(0x03), 9, READ_NOW_MS),
        Err(ArtifactError::RevisionNotFound { revision: 9, .. })
    ));
    assert!(matches!(
        store.inspect_revision(artifact_id(0x03), 9),
        Err(ArtifactError::RevisionNotFound { revision: 9, .. })
    ));
}

#[test]
fn durability_pragmas_are_wal_and_full() {
    let directory = TestStoreDir::new("durability");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x04)).expect("create");
    drop(store);

    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw open");
    let journal_mode: String = raw
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .expect("read journal_mode");
    let synchronous: i64 = raw
        .pragma_query_value(None, "synchronous", |row| row.get(0))
        .expect("read synchronous");
    assert!(journal_mode.eq_ignore_ascii_case("wal"));
    assert_eq!(synchronous, 2, "synchronous=FULL");
    let user_version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read user_version");
    assert_eq!(user_version, 8);
}

#[test]
fn artifact_head_endpoint_proof_is_authority_assigned_durable_and_immutable() {
    let directory = TestStoreDir::new("endpoint-proof");
    let proof = {
        let store = ArtifactStore::open(directory.root()).expect("open");
        let spec = artifact_spec(0x31);
        store.create_artifact(spec.clone()).expect("create");
        let proof = store
            .inspect_head_endpoint_proof(spec.artifact_id)
            .expect("proof");
        assert_eq!(proof.artifact_id, spec.artifact_id);
        assert_eq!(
            proof.participant_generation,
            nlos_types::Generation::INITIAL
        );
        proof
    };

    let store = ArtifactStore::open(directory.root()).expect("reopen");
    assert_eq!(
        store
            .inspect_head_endpoint_proof(proof.artifact_id)
            .expect("durable proof"),
        proof
    );
    drop(store);
    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw");
    assert!(
        raw.execute(
            "UPDATE artifact_head_endpoint_proofs SET participant_id=zeroblob(16)",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM artifact_head_endpoint_proofs", [])
            .is_err()
    );
}

#[test]
fn unknown_schema_version_fails_closed() {
    let directory = TestStoreDir::new("schema-gate");
    let store = ArtifactStore::open(directory.root()).expect("open");
    drop(store);
    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw open");
    raw.pragma_update(None, "user_version", 99)
        .expect("bump user_version");
    drop(raw);

    let result = ArtifactStore::open(directory.root());
    let Err(error) = result else {
        panic!("unknown schema version must fail closed");
    };
    assert!(matches!(error, ArtifactError::SchemaVersionUnsupported(99)));
}

/// deep-audit/32 M1 regression: a blob already sitting at the digest
/// address is only a benign no-op when its stored bytes actually hash to
/// that address. A corrupted (bit-rotted or truncated) blob must make the
/// commit fail with a typed `DigestMismatch` instead of being silently
/// reused — previously every put of this digest reported success while
/// every read failed forever with no commit-time diagnosis. No metadata
/// is committed on the failure; repairing the file unblocks the same put.
#[test]
fn put_over_corrupted_existing_blob_fails_typed_and_commits_no_metadata() {
    let directory = TestStoreDir::new("already-present-corrupt");
    let store = ArtifactStore::open(directory.root()).expect("open store");
    store.create_artifact(artifact_spec(0x0f)).expect("create");

    // Plant corrupted bytes at the exact content address of the payload.
    let payload = bytes(0x2a, 128);
    let digest = nlos_artifact::ContentDigest::of_bytes(&payload);
    let blob_path = directory.artifact_blob(digest);
    std::fs::create_dir_all(blob_path.parent().expect("shard dir")).expect("shard dir");
    std::fs::write(&blob_path, b"corrupted bytes parked at the digest address")
        .expect("plant corrupted blob");

    let error = store
        .put_revision(put(artifact_id(0x0f), 0, &payload))
        .expect_err("commit over a corrupted blob must fail closed");
    assert!(
        matches!(error, ArtifactError::DigestMismatch { expected, .. } if expected == digest),
        "expected DigestMismatch for {digest}, got {error}"
    );

    // Fail-closed means fail-closed on the metadata plane too: no
    // revision, no head advance.
    assert!(matches!(
        store.get_revision(artifact_id(0x0f), 1, READ_NOW_MS),
        Err(ArtifactError::RevisionNotFound { .. })
    ));
    assert_eq!(
        store
            .resolve_head(artifact_id(0x0f), READ_NOW_MS)
            .expect("head"),
        None
    );

    // Repairing the stored bytes (the operator remedy the typed error
    // points at) makes the identical put succeed and read back.
    std::fs::write(&blob_path, &payload).expect("repair blob");
    match store
        .put_revision(put(artifact_id(0x0f), 0, &payload))
        .expect("put after repair")
    {
        PutRevisionDecision::Committed(record) => assert_eq!(record.digest, digest),
        PutRevisionDecision::Replayed(_) => panic!("first successful put must commit"),
    }
    assert_eq!(
        store
            .get_revision(artifact_id(0x0f), 1, READ_NOW_MS)
            .expect("get after repair"),
        payload
    );
}
