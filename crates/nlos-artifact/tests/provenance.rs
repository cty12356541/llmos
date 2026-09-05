//! B-ARTIFACT-006 provenance minimal prefix: per-revision immutable
//! receipts, caller-asserted vs owner-derived source triples, fail-closed
//! byte reads, and audit-plane inspectability.

mod support;

use std::fs;

use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, ProvenanceSourceKind,
    PublishStagedRevisionDecision, PutRevisionDecision,
};
use nlos_types::{CommitPermitId, IdempotencyKey, TaskId};
use rusqlite::Connection;
use support::{
    READ_NOW_MS, TestStoreDir, artifact_id, artifact_spec, bytes, provenance_triple, put,
};

fn stage_request(
    seed: u8,
    expected_head_revision: u64,
    payload: &[u8],
) -> nlos_artifact::StageRevisionRequest<'_> {
    nlos_artifact::StageRevisionRequest {
        artifact_id: artifact_id(seed),
        expected_head_revision,
        bytes: payload,
        task_id: TaskId::from_bytes([0x31 + seed; 16]),
        permit_id: CommitPermitId::from_bytes([0x51 + seed; 16]),
        write_set_root: ContentDigest::from_bytes([0x71 + seed; 32]),
        idempotency_key: IdempotencyKey::from_bytes([0x91 + seed; 16]),
        created_at_ms: 7_000 + u64::from(seed),
    }
}

fn publish_request(
    staged: &nlos_artifact::StagedRevisionRecord,
) -> nlos_artifact::PublishStagedRevisionRequest {
    nlos_artifact::PublishStagedRevisionRequest {
        staging_id: staged.staging_id,
        task_id: staged.task_id,
        permit_id: staged.permit_id,
        write_set_root: staged.write_set_root,
        published_at_ms: 8_000,
    }
}

#[test]
fn put_records_caller_asserted_provenance_and_reads_require_it() {
    let directory = TestStoreDir::new("caller-provenance");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let spec = artifact_spec(1);
    store.create_artifact(spec.clone()).expect("create");
    let payload = bytes(0x11, 128);
    let triple = provenance_triple(0x01);

    let committed = match store
        .put_revision(nlos_artifact::PutRevisionRequest {
            provenance: triple,
            ..put(spec.artifact_id, 0, &payload)
        })
        .expect("put")
    {
        PutRevisionDecision::Committed(record) => record,
        PutRevisionDecision::Replayed(_) => panic!("first put must commit"),
    };

    let provenance = store
        .inspect_provenance(spec.artifact_id, 1)
        .expect("inspect provenance");
    assert_eq!(provenance.artifact_id, spec.artifact_id);
    assert_eq!(provenance.revision, committed.revision);
    assert_eq!(provenance.source_kind, ProvenanceSourceKind::CallerAssertedOpaque);
    assert_eq!(provenance.source_triple, triple);
    assert_eq!(provenance.publication_receipt_id, None);

    assert_eq!(
        store
            .get_revision(spec.artifact_id, 1, READ_NOW_MS)
            .expect("get"),
        payload
    );
    assert_eq!(
        store
            .inspect_provenance_receipt(provenance.receipt_id)
            .expect("by id"),
        provenance
    );
}

#[test]
fn publish_records_owner_derived_provenance_bound_to_publication_receipt() {
    let directory = TestStoreDir::new("owner-provenance");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(2)).expect("create");
    let payload = bytes(0x22, 64);
    let staged = store
        .stage_revision(stage_request(2, 0, &payload))
        .expect("stage")
        .record()
        .clone();
    let publication = match store.publish_staged_revision(publish_request(&staged)) {
        Ok(nlos_artifact::PublishStagedRevisionDecision::Published(receipt)) => receipt,
        other => panic!("expected published, got {other:?}"),
    };

    let provenance = store
        .inspect_provenance(artifact_id(2), 1)
        .expect("inspect");
    assert_eq!(provenance.source_kind, ProvenanceSourceKind::OwnerDerived);
    assert_eq!(
        provenance.source_triple.source_a,
        staged.task_id.into_bytes()
    );
    assert_eq!(
        provenance.source_triple.source_b,
        staged.permit_id.into_bytes()
    );
    assert_eq!(provenance.source_triple.source_digest, staged.write_set_root);
    assert_eq!(
        provenance.publication_receipt_id,
        Some(publication.receipt_id)
    );
    assert_eq!(
        store
            .get_revision(artifact_id(2), 1, READ_NOW_MS)
            .expect("get"),
        payload
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One test covers v6→v7 migration + fail-closed gate + audit plane.
fn revision_without_provenance_fails_closed_on_byte_read_but_metadata_inspectable() {
    let directory = TestStoreDir::new("missing-provenance");
    fs::create_dir_all(directory.root()).expect("root");
    let database = directory.root().join("metadata.db");
    let payload = bytes(0x33, 16);
    let digest = ContentDigest::of_bytes(&payload);
    fs::create_dir_all(
        directory
            .root()
            .join("artifacts/blobs")
            .join(&digest.to_hex()[..2]),
    )
    .expect("dirs");
    fs::write(directory.artifact_blob(digest), &payload).expect("blob");

    let connection = Connection::open(&database).expect("open raw v6");
    let spec = artifact_spec(3);
    connection
        .execute_batch(
            "CREATE TABLE artifacts (
                artifact_id BLOB PRIMARY KEY NOT NULL CHECK(length(artifact_id) = 16),
                idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
                content_type TEXT NOT NULL,
                application_id BLOB,
                owner TEXT,
                head_revision INTEGER NOT NULL DEFAULT 0,
                head_digest BLOB,
                created_at_ms INTEGER NOT NULL,
                retention_ms INTEGER
            ) STRICT;
            CREATE TABLE artifact_revisions (
                artifact_id BLOB NOT NULL,
                revision INTEGER NOT NULL,
                digest BLOB NOT NULL CHECK(length(digest) = 32),
                size_bytes INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY(artifact_id, revision)
            ) STRICT;
            CREATE TABLE cache_entries (
                cache_key TEXT PRIMARY KEY NOT NULL,
                digest BLOB NOT NULL,
                size_bytes INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE artifact_head_endpoint_proofs (
                artifact_id BLOB PRIMARY KEY NOT NULL,
                participant_id BLOB NOT NULL UNIQUE,
                participant_generation BLOB NOT NULL,
                admission_receipt_id BLOB NOT NULL UNIQUE
            ) STRICT;
            CREATE TABLE artifact_staged_revisions (
                staging_id BLOB PRIMARY KEY NOT NULL,
                idempotency_key BLOB NOT NULL UNIQUE,
                artifact_id BLOB NOT NULL,
                expected_head_revision INTEGER NOT NULL,
                target_revision INTEGER NOT NULL,
                digest BLOB NOT NULL,
                size_bytes INTEGER NOT NULL,
                task_id BLOB NOT NULL,
                permit_id BLOB NOT NULL,
                write_set_root BLOB NOT NULL,
                stage_state INTEGER NOT NULL,
                publication_receipt_id BLOB,
                created_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE artifact_publication_receipts (
                receipt_id BLOB PRIMARY KEY NOT NULL,
                staging_id BLOB NOT NULL UNIQUE,
                artifact_id BLOB NOT NULL,
                revision INTEGER NOT NULL,
                digest BLOB NOT NULL,
                size_bytes INTEGER NOT NULL,
                task_id BLOB NOT NULL,
                permit_id BLOB NOT NULL,
                write_set_root BLOB NOT NULL,
                prior_head_revision INTEGER NOT NULL,
                prior_head_digest BLOB,
                new_head_revision INTEGER NOT NULL,
                new_head_digest BLOB NOT NULL,
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE package_verification_receipts (
                receipt_id BLOB PRIMARY KEY NOT NULL,
                idempotency_key BLOB NOT NULL UNIQUE,
                manifest_digest BLOB NOT NULL,
                package_id BLOB NOT NULL,
                package_version INTEGER NOT NULL,
                entry_count INTEGER NOT NULL,
                signer_principal BLOB NOT NULL,
                signer_key_id BLOB NOT NULL,
                signer_key_generation INTEGER NOT NULL,
                signature BLOB NOT NULL,
                verified_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE artifact_gc_receipts (
                receipt_id BLOB PRIMARY KEY NOT NULL,
                idempotency_key BLOB NOT NULL UNIQUE,
                collected_digests BLOB NOT NULL,
                collected_count INTEGER NOT NULL,
                scanned_blob_count INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            PRAGMA user_version = 6;",
        )
        .expect("v6 schema");
    connection
        .execute(
            "INSERT INTO artifacts (
                artifact_id, idempotency_key, content_type, application_id,
                owner, head_revision, head_digest, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)",
            rusqlite::params![
                spec.artifact_id.as_bytes().as_slice(),
                spec.idempotency_key.as_bytes().as_slice(),
                spec.content_type,
                spec.application_id.map(nlos_types::ApplicationId::into_bytes),
                spec.owner,
                digest.as_bytes().as_slice(),
                i64::try_from(spec.created_at_ms).unwrap(),
            ],
        )
        .expect("artifact");
    connection
        .execute(
            "INSERT INTO artifact_revisions (
                artifact_id, revision, digest, size_bytes, created_at_ms
             ) VALUES (?1, 1, ?2, 16, 1000)",
            rusqlite::params![
                spec.artifact_id.as_bytes().as_slice(),
                digest.as_bytes().as_slice(),
            ],
        )
        .expect("revision");
    connection
        .execute(
            "INSERT INTO artifact_head_endpoint_proofs (
                artifact_id, participant_id, participant_generation, admission_receipt_id
             ) VALUES (?1, randomblob(16), X'0000000000000001', randomblob(16))",
            [spec.artifact_id.as_bytes().as_slice()],
        )
        .expect("endpoint");
    drop(connection);

    let store = ArtifactStore::open(directory.root()).expect("migrate v6 to v7");
    assert!(store.inspect_revision(spec.artifact_id, 1).is_ok());
    assert!(matches!(
        store.inspect_provenance(spec.artifact_id, 1),
        Err(ArtifactError::ProvenanceIncomplete { .. })
    ));
    assert!(matches!(
        store.get_revision(spec.artifact_id, 1, READ_NOW_MS),
        Err(ArtifactError::ProvenanceIncomplete { .. })
    ));
    assert!(store.resolve_head(spec.artifact_id, READ_NOW_MS).unwrap().is_some());
}

#[test]
fn provenance_receipt_is_immutable_and_survives_restart() {
    let directory = TestStoreDir::new("provenance-restart");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let spec = artifact_spec(4);
    store.create_artifact(spec.clone()).expect("create");
    let payload = bytes(0x44, 32);
    store.put_revision(put(spec.artifact_id, 0, &payload)).expect("put");
    let provenance = store
        .inspect_provenance(spec.artifact_id, 1)
        .expect("inspect");
    let receipt_id = provenance.receipt_id;
    drop(store);

    let raw = Connection::open(directory.root().join("metadata.db")).unwrap();
    assert!(raw
        .execute("UPDATE artifact_provenance_receipts SET created_at_ms=0", [])
        .is_err());
    assert!(raw
        .execute("DELETE FROM artifact_provenance_receipts", [])
        .is_err());
    drop(raw);

    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    assert_eq!(
        reopened
            .inspect_provenance_receipt(receipt_id)
            .expect("replay"),
        provenance
    );
    assert_eq!(
        reopened
            .get_revision(spec.artifact_id, 1, READ_NOW_MS)
            .expect("read"),
        payload
    );
}

#[test]
fn put_replay_preserves_provenance_and_does_not_duplicate_rows() {
    let directory = TestStoreDir::new("provenance-replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let spec = artifact_spec(5);
    store.create_artifact(spec.clone()).expect("create");
    let payload = bytes(0x55, 48);
    let request = put(spec.artifact_id, 0, &payload);
    let first = store.put_revision(request).expect("first");
    let replay = store.put_revision(request).expect("replay");
    assert!(matches!(replay, PutRevisionDecision::Replayed(_)));
    assert_eq!(first.record(), replay.record());
    let provenance = store
        .inspect_provenance(spec.artifact_id, 1)
        .expect("inspect");
    let count: i64 = Connection::open(directory.root().join("metadata.db"))
        .unwrap()
        .query_row(
            "SELECT count(*) FROM artifact_provenance_receipts WHERE artifact_id=?1",
            [spec.artifact_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(provenance.source_triple, request.provenance);
}

#[test]
fn staged_publication_replay_does_not_duplicate_provenance() {
    let directory = TestStoreDir::new("publish-provenance-replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(6)).expect("create");
    let payload = bytes(0x66, 40);
    let staged = store
        .stage_revision(stage_request(6, 0, &payload))
        .expect("stage")
        .record()
        .clone();
    let first = store
        .publish_staged_revision(publish_request(&staged))
        .expect("publish");
    let replay = store
        .publish_staged_revision(publish_request(&staged))
        .expect("replay");
    assert!(matches!(
        replay,
        PublishStagedRevisionDecision::Replayed(_)
    ));
    let provenance = store
        .inspect_provenance(artifact_id(6), 1)
        .expect("inspect");
    assert_eq!(
        provenance.publication_receipt_id,
        Some(first.receipt().receipt_id)
    );
    assert_eq!(provenance.source_kind, ProvenanceSourceKind::OwnerDerived);
}
