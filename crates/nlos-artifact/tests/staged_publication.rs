mod support;

use std::fs;

use nlos_artifact::{
    ArtifactError, ArtifactStore, ContentDigest, PublishStagedRevisionDecision,
    PublishStagedRevisionRequest, StageRevisionDecision, StageRevisionRequest, StagedRevisionState,
};
use nlos_types::{CommitPermitId, IdempotencyKey, TaskId};
use rusqlite::Connection;

use support::{TestStoreDir, artifact_id, artifact_spec, bytes};

fn stage_request(
    seed: u8,
    expected_head_revision: u64,
    payload: &[u8],
) -> StageRevisionRequest<'_> {
    StageRevisionRequest {
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

fn publish_request(staged: &nlos_artifact::StagedRevisionRecord) -> PublishStagedRevisionRequest {
    PublishStagedRevisionRequest {
        staging_id: staged.staging_id,
        task_id: staged.task_id,
        permit_id: staged.permit_id,
        write_set_root: staged.write_set_root,
        published_at_ms: 8_000,
    }
}

#[test]
fn staging_is_durable_but_does_not_advance_canonical_head() {
    let directory = TestStoreDir::new("stage-no-head");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(1);
    store.create_artifact(artifact.clone()).expect("create");
    let payload = bytes(0x11, 257);

    let decision = store
        .stage_revision(stage_request(1, 0, &payload))
        .expect("stage");
    let staged = decision.record();
    assert!(matches!(decision, StageRevisionDecision::Staged(_)));
    assert_eq!(staged.state, StagedRevisionState::Staged);
    assert_eq!(staged.target_revision, 1);
    assert_eq!(
        store.resolve_head(artifact.artifact_id).expect("head"),
        None
    );
    assert!(matches!(
        store.inspect_revision(artifact.artifact_id, 1),
        Err(ArtifactError::RevisionNotFound { .. })
    ));

    let report = store.recover().expect("recover");
    assert!(report.missing_staged_blobs.is_empty());
    assert!(report.orphan_blobs.is_empty());
}

#[test]
fn stage_is_exactly_replayable_and_rejects_key_rebinding() {
    let directory = TestStoreDir::new("stage-replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(2)).expect("create");
    let payload = bytes(0x22, 89);

    let first = store
        .stage_revision(stage_request(2, 0, &payload))
        .expect("first stage");
    let replay = store
        .stage_revision(stage_request(2, 0, &payload))
        .expect("replay stage");
    assert!(matches!(replay, StageRevisionDecision::Replayed(_)));
    assert_eq!(first.record(), replay.record());

    let changed = bytes(0x23, 89);
    assert!(matches!(
        store.stage_revision(stage_request(2, 0, &changed)),
        Err(ArtifactError::IdempotencyConflict)
    ));
    let mut rebound = stage_request(2, 0, &payload);
    rebound.task_id = TaskId::from_bytes([0xee; 16]);
    assert!(matches!(
        store.stage_revision(rebound),
        Err(ArtifactError::IdempotencyConflict)
    ));
}

#[test]
fn publication_atomically_advances_head_and_replays_immutable_receipt() {
    let directory = TestStoreDir::new("publish-replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(3);
    store.create_artifact(artifact.clone()).expect("create");
    let payload = bytes(0x33, 513);
    let staged = store
        .stage_revision(stage_request(3, 0, &payload))
        .expect("stage")
        .record()
        .clone();

    let mut rebound = publish_request(&staged);
    rebound.permit_id = CommitPermitId::from_bytes([0xff; 16]);
    assert!(matches!(
        store.publish_staged_revision(rebound),
        Err(ArtifactError::PublicationBindingMismatch)
    ));
    assert_eq!(store.resolve_head(artifact.artifact_id).unwrap(), None);

    let first = store
        .publish_staged_revision(publish_request(&staged))
        .expect("publish");
    assert!(matches!(first, PublishStagedRevisionDecision::Published(_)));
    let receipt = first.receipt();
    assert_eq!(receipt.prior_head_revision, 0);
    assert_eq!(receipt.prior_head_digest, None);
    assert_eq!(receipt.new_head_revision, 1);
    assert_eq!(
        store.get_revision(artifact.artifact_id, 1).unwrap(),
        payload
    );
    assert_eq!(
        store
            .inspect_publication_receipt(receipt.receipt_id)
            .unwrap(),
        *receipt
    );

    let replay = store
        .publish_staged_revision(publish_request(&staged))
        .expect("publish replay");
    assert!(matches!(replay, PublishStagedRevisionDecision::Replayed(_)));
    assert_eq!(first.receipt(), replay.receipt());
    let current = store
        .inspect_staged_revision(staged.staging_id)
        .expect("inspect stage");
    assert_eq!(current.state, StagedRevisionState::Published);
    assert_eq!(current.publication_receipt_id, Some(receipt.receipt_id));

    let receipt_id = receipt.receipt_id;
    drop(store);
    let raw = Connection::open(directory.root().join("metadata.db")).expect("open raw");
    assert!(
        raw.execute(
            "UPDATE artifact_publication_receipts SET created_at_ms = created_at_ms + 1
             WHERE receipt_id = ?1",
            [receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM artifact_publication_receipts WHERE receipt_id = ?1",
            [receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn competing_staged_revisions_have_exactly_one_publish_winner() {
    let directory = TestStoreDir::new("publish-race");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(4)).expect("create");
    let left_bytes = bytes(0x41, 64);
    let right_bytes = bytes(0x42, 64);
    let left = store
        .stage_revision(stage_request(4, 0, &left_bytes))
        .unwrap()
        .record()
        .clone();
    let mut right_request = stage_request(4, 0, &right_bytes);
    right_request.idempotency_key = IdempotencyKey::from_bytes([0xf4; 16]);
    let right = store
        .stage_revision(right_request)
        .unwrap()
        .record()
        .clone();

    store
        .publish_staged_revision(publish_request(&left))
        .expect("winner publishes");
    assert!(matches!(
        store.publish_staged_revision(publish_request(&right)),
        Err(ArtifactError::HeadConflict {
            expected: 0,
            current: 1
        })
    ));
    assert_eq!(
        store
            .inspect_staged_revision(right.staging_id)
            .unwrap()
            .state,
        StagedRevisionState::Staged
    );
    assert_eq!(store.list_revisions(artifact_id(4)).unwrap().len(), 1);
}

#[test]
fn restart_preserves_stage_and_publication_replay() {
    let directory = TestStoreDir::new("restart");
    let payload = bytes(0x55, 127);
    let staged = {
        let store = ArtifactStore::open(directory.root()).expect("open");
        store.create_artifact(artifact_spec(5)).expect("create");
        store
            .stage_revision(stage_request(5, 0, &payload))
            .expect("stage")
            .record()
            .clone()
    };
    let receipt = {
        let reopened = ArtifactStore::open(directory.root()).expect("reopen");
        assert_eq!(
            reopened.inspect_staged_revision(staged.staging_id).unwrap(),
            staged
        );
        reopened
            .publish_staged_revision(publish_request(&staged))
            .expect("publish after restart")
            .receipt()
            .clone()
    };
    let reopened = ArtifactStore::open(directory.root()).expect("second reopen");
    let replay = reopened
        .publish_staged_revision(publish_request(&staged))
        .expect("replay after restart");
    assert_eq!(replay.receipt(), &receipt);
}

#[test]
fn missing_staged_blob_is_visible_and_blocks_publication() {
    let directory = TestStoreDir::new("missing-stage");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(6)).expect("create");
    let payload = bytes(0x66, 73);
    let staged = store
        .stage_revision(stage_request(6, 0, &payload))
        .unwrap()
        .record()
        .clone();
    fs::remove_file(directory.artifact_blob(staged.digest)).expect("remove staged blob");

    let report = store.recover().expect("recover");
    assert_eq!(report.missing_staged_blobs.len(), 1);
    assert_eq!(report.missing_staged_blobs[0].staging_id, staged.staging_id);
    assert!(report.missing_blobs.is_empty());
    assert!(matches!(
        store.publish_staged_revision(publish_request(&staged)),
        Err(ArtifactError::StagedBlobMissing { .. })
    ));
    assert_eq!(store.resolve_head(artifact_id(6)).unwrap(), None);
}

#[test]
fn v1_store_migrates_to_v2_without_losing_artifacts() {
    let directory = TestStoreDir::new("v1-v2");
    fs::create_dir_all(directory.root()).expect("create root");
    let database = directory.root().join("metadata.db");
    let connection = Connection::open(&database).expect("open raw v1");
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
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE TABLE artifact_revisions (
                artifact_id BLOB NOT NULL,
                revision INTEGER NOT NULL,
                digest BLOB NOT NULL,
                size_bytes INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                PRIMARY KEY(artifact_id, revision),
                FOREIGN KEY(artifact_id) REFERENCES artifacts(artifact_id)
            ) STRICT;
            CREATE TABLE cache_entries (
                cache_key TEXT PRIMARY KEY NOT NULL,
                digest BLOB NOT NULL,
                size_bytes INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            PRAGMA user_version = 1;",
        )
        .expect("create v1 schema");
    let spec = artifact_spec(7);
    connection
        .execute(
            "INSERT INTO artifacts (
                artifact_id, idempotency_key, content_type, application_id,
                owner, head_revision, head_digest, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 0, NULL, ?6)",
            rusqlite::params![
                spec.artifact_id.as_bytes().as_slice(),
                spec.idempotency_key.as_bytes().as_slice(),
                spec.content_type,
                spec.application_id
                    .map(nlos_types::ApplicationId::into_bytes),
                spec.owner,
                i64::try_from(spec.created_at_ms).unwrap(),
            ],
        )
        .expect("insert v1 artifact");
    drop(connection);

    let store = ArtifactStore::open(directory.root()).expect("migrate v1 to v2");
    assert_eq!(
        store
            .inspect_artifact(spec.artifact_id)
            .unwrap()
            .artifact_id,
        spec.artifact_id
    );
    drop(store);
    let connection = Connection::open(database).expect("reopen raw");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 4);
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM artifact_head_endpoint_proofs WHERE artifact_id=?1",
                [spec.artifact_id.as_bytes().as_slice()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1
    );
    let table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master
             WHERE type = 'table' AND name IN (
                'artifact_staged_revisions', 'artifact_publication_receipts'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 2);
}
