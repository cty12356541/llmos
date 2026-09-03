//! B-ARTIFACT-005 retention policy minimal prefix: per-artifact time
//! upper bound — durable/idempotent setting, fail-closed read and
//! admission rejection past the deadline, replay semantics, restart
//! persistence, boundary behaviour, and the mark-only coordination with
//! the orphan GC (expired-but-referenced blobs are never collected).

use nlos_artifact::{
    ArtifactError, ArtifactStore, CollectOrphanBlobsDecision, CollectOrphanBlobsRequest,
    ContentDigest, PublishStagedRevisionDecision, PublishStagedRevisionRequest,
    PutRevisionDecision, PutRevisionRequest, SetRetentionDecision, SetRetentionRequest,
    StageRevisionDecision, StageRevisionRequest, staging_id_for,
};
use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskId};
use support::{TestStoreDir, artifact_spec, bytes, put};

mod support;

// artifact_spec(0) carries created_at_ms = 1_000; a retention of 5_000 ms
// makes the absolute deadline 6_000 ms. artifact_spec(4) is anchored at
// 1_004 ms.
const CREATED_AT_MS: u64 = 1_000;
const RETENTION_MS: u64 = 5_000;
const DEADLINE_MS: u64 = CREATED_AT_MS + RETENTION_MS;
const CREATED_AT_MS_ZERO: u64 = 1_004;

fn set_retention(artifact: ArtifactId, retention_ms: u64) -> SetRetentionRequest {
    SetRetentionRequest {
        artifact_id: artifact,
        retention_ms,
    }
}

fn stage_request(
    artifact: ArtifactId,
    expected_head: u64,
    payload: &[u8],
    key: u8,
    created_at_ms: u64,
) -> StageRevisionRequest<'_> {
    StageRevisionRequest {
        artifact_id: artifact,
        expected_head_revision: expected_head,
        bytes: payload,
        task_id: TaskId::from_bytes([0x81; 16]),
        permit_id: CommitPermitId::from_bytes([0x82; 16]),
        write_set_root: ContentDigest::of_bytes(b"retention-write-set"),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        created_at_ms,
    }
}

fn publish_request(
    staging_id: nlos_artifact::StagingId,
    published_at_ms: u64,
) -> PublishStagedRevisionRequest {
    PublishStagedRevisionRequest {
        staging_id,
        task_id: TaskId::from_bytes([0x81; 16]),
        permit_id: CommitPermitId::from_bytes([0x82; 16]),
        write_set_root: ContentDigest::of_bytes(b"retention-write-set"),
        published_at_ms,
    }
}

/// The `put` support helper always stamps inside the readable window;
/// this variant stamps strictly past the deadline.
fn put_after_deadline(
    artifact: ArtifactId,
    expected_head: u64,
    payload: &[u8],
) -> PutRevisionRequest<'_> {
    PutRevisionRequest {
        artifact_id: artifact,
        expected_head_revision: expected_head,
        bytes: payload,
        created_at_ms: DEADLINE_MS + 1,
    }
}

fn assert_expired(error: &ArtifactError, artifact: ArtifactId, expires_at_ms: u64) {
    match error {
        ArtifactError::RetentionExpired {
            artifact_id,
            expires_at_ms: deadline,
        } => {
            assert_eq!(*artifact_id, artifact);
            assert_eq!(*deadline, expires_at_ms);
        }
        other => panic!("expected RetentionExpired, got {other}"),
    }
}

/// Setting is durable, idempotent under repetition, changeable in both
/// directions, validates its inputs, and the stored state (including the
/// derived absolute deadline) survives a restart.
#[test]
fn set_retention_is_durable_idempotent_and_changeable() {
    let directory = TestStoreDir::new("retention-set");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(0).artifact_id;
    store.create_artifact(artifact_spec(0)).expect("create");

    // Initially unbounded: no policy, no deadline.
    let initial = store.inspect_retention(artifact).expect("inspect initial");
    assert_eq!(initial.retention_ms, None);
    assert_eq!(initial.expires_at_ms, None);
    assert_eq!(initial.created_at_ms, CREATED_AT_MS);

    let first = store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("first set");
    assert!(matches!(first, SetRetentionDecision::Updated(_)));
    assert_eq!(first.record().retention_ms, Some(RETENTION_MS));
    assert_eq!(first.record().expires_at_ms, Some(DEADLINE_MS));

    // Same value replays without change.
    let replay = store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("replay set");
    assert!(matches!(replay, SetRetentionDecision::Replayed(_)));
    assert_eq!(replay.record(), first.record());

    // Extension and shrink both update; the new bound is the current one.
    let extended = store
        .set_retention(set_retention(artifact, RETENTION_MS * 2))
        .expect("extend");
    assert!(matches!(extended, SetRetentionDecision::Updated(_)));
    assert_eq!(
        extended.record().expires_at_ms,
        Some(DEADLINE_MS + RETENTION_MS)
    );
    let shrunk = store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("shrink");
    assert_eq!(shrunk.record().expires_at_ms, Some(DEADLINE_MS));

    // Validation: unknown artifact and a bound that cannot be stored.
    let unknown = ArtifactId::from_bytes([0xEE; 16]);
    assert!(matches!(
        store.set_retention(set_retention(unknown, RETENTION_MS)),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
    assert!(matches!(
        store.set_retention(set_retention(artifact, i64::MAX as u64 + 1)),
        Err(ArtifactError::InvalidSpec(_))
    ));

    // Durability across restart.
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    let stored = reopened.inspect_retention(artifact).expect("inspect after");
    assert_eq!(stored.retention_ms, Some(RETENTION_MS));
    assert_eq!(stored.expires_at_ms, Some(DEADLINE_MS));
}

/// The boundary is half-open: readable while `now_ms <= deadline`
/// (including exactly at the deadline), expired strictly after. Reads and
/// polls fail closed with the typed expiry error carrying the deadline;
/// unbounded artifacts never expire at any observation time.
#[test]
fn expiry_boundary_is_half_open_and_reads_fail_closed() {
    let directory = TestStoreDir::new("retention-reads");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(0).artifact_id;
    store.create_artifact(artifact_spec(0)).expect("create");
    let payload = bytes(0x5A, 256);
    store
        .put_revision(put(artifact, 0, &payload))
        .expect("put revision");
    store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("set retention");

    // Exactly at the deadline: still readable (both read and poll).
    assert_eq!(
        store
            .get_revision(artifact, 1, DEADLINE_MS)
            .expect("read at deadline"),
        payload
    );
    assert!(
        store
            .resolve_head(artifact, DEADLINE_MS)
            .expect("poll at deadline")
            .is_some()
    );

    // Strictly after: typed, fail-closed rejection on both surfaces.
    let read_error = store
        .get_revision(artifact, 1, DEADLINE_MS + 1)
        .expect_err("expired read must fail");
    assert_expired(&read_error, artifact, DEADLINE_MS);
    let poll_error = store
        .resolve_head(artifact, DEADLINE_MS + 1)
        .expect_err("expired poll must fail");
    assert_expired(&poll_error, artifact, DEADLINE_MS);

    // Expiry is distinct from not-found: the metadata plane stays
    // inspectable and the blob stays on disk (refusal only, no deletion).
    let inspected = store.inspect_artifact(artifact).expect("inspect artifact");
    assert_eq!(inspected.retention_ms, Some(RETENTION_MS));
    store
        .inspect_revision(artifact, 1)
        .expect("inspect revision");
    store.list_revisions(artifact).expect("list revisions");
    assert!(
        directory
            .artifact_blob(ContentDigest::of_bytes(&payload))
            .is_file(),
        "expired blob must not be deleted"
    );

    // An expired artifact with no revisions fails the poll too: the gate
    // is on the artifact, not on its head. (Re-anchored to CREATED_AT_MS
    // so the deadline arithmetic matches DEADLINE_MS.)
    let mut empty_spec = artifact_spec(2);
    empty_spec.created_at_ms = CREATED_AT_MS;
    let empty = empty_spec.artifact_id;
    store.create_artifact(empty_spec).expect("create empty");
    store
        .set_retention(set_retention(empty, RETENTION_MS))
        .expect("set retention on empty");
    let empty_error = store
        .resolve_head(empty, DEADLINE_MS + 1)
        .expect_err("expired empty artifact must fail the poll");
    assert_expired(&empty_error, empty, DEADLINE_MS);

    // Unbounded artifacts never expire at any observation time.
    let unbounded = artifact_spec(3).artifact_id;
    store
        .create_artifact(artifact_spec(3))
        .expect("create unbounded");
    let other = bytes(0x5B, 64);
    store
        .put_revision(put(unbounded, 0, &other))
        .expect("put unbounded");
    assert_eq!(
        store
            .get_revision(unbounded, 1, u64::MAX)
            .expect("unbounded read at u64::MAX"),
        other
    );
}

/// Content admission (fresh put/stage/publish) is refused past the
/// deadline at the request's own timestamp, while replays of
/// already-committed durable facts stay un-gated.
#[test]
fn expired_artifact_refuses_fresh_admission_but_replays_durable_facts() {
    let directory = TestStoreDir::new("retention-admission");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(0).artifact_id;
    store.create_artifact(artifact_spec(0)).expect("create");

    let payload1 = bytes(0x71, 128);
    store
        .put_revision(put(artifact, 0, &payload1))
        .expect("put revision 1");

    // Stage a candidate while still readable (exactly at the deadline),
    // and publish it exactly at the deadline.
    let staged_payload = bytes(0x72, 128);
    store
        .stage_revision(stage_request(
            artifact,
            1,
            &staged_payload,
            0x91,
            DEADLINE_MS,
        ))
        .expect("stage before deadline");
    store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("set retention");
    let publication = store
        .publish_staged_revision(publish_request(
            staging_id_for(artifact, IdempotencyKey::from_bytes([0x91; 16])),
            DEADLINE_MS,
        ))
        .expect("publish at deadline");
    let receipt = publication.receipt();
    assert_eq!(receipt.new_head_revision, 2);

    // Fresh admission past the deadline is refused on the put and stage
    // paths.
    let payload2 = bytes(0x73, 128);
    let put_error = store
        .put_revision(put_after_deadline(artifact, 2, &payload2))
        .expect_err("fresh put after deadline");
    assert_expired(&put_error, artifact, DEADLINE_MS);
    let stage_error = store
        .stage_revision(stage_request(artifact, 2, &payload2, 0x92, DEADLINE_MS + 1))
        .expect_err("fresh stage after deadline");
    assert_expired(&stage_error, artifact, DEADLINE_MS);

    // Stage a second candidate before the deadline, let the deadline
    // pass, and verify fresh publication is refused.
    let staged2 = bytes(0x74, 128);
    store
        .stage_revision(stage_request(artifact, 2, &staged2, 0x93, DEADLINE_MS))
        .expect("stage second candidate at deadline");
    let publish2_error = store
        .publish_staged_revision(publish_request(
            staging_id_for(artifact, IdempotencyKey::from_bytes([0x93; 16])),
            DEADLINE_MS + 1,
        ))
        .expect_err("fresh publish after deadline");
    assert_expired(&publish2_error, artifact, DEADLINE_MS);

    // Replays of committed facts stay un-gated even past the deadline:
    // exact re-put of revision 1, exact re-stage, and the publication
    // replay all return durable history without creating state.
    let replayed_put = store
        .put_revision(put(artifact, 0, &payload1))
        .expect("re-put of committed revision replays");
    assert!(matches!(replayed_put, PutRevisionDecision::Replayed(_)));
    let replayed_stage = store
        .stage_revision(stage_request(artifact, 2, &staged2, 0x93, DEADLINE_MS + 1))
        .expect("re-stage replays");
    assert!(matches!(replayed_stage, StageRevisionDecision::Replayed(_)));
    let replayed_publish = store
        .publish_staged_revision(publish_request(receipt.staging_id, DEADLINE_MS + 1))
        .expect("publication replay");
    assert!(matches!(
        replayed_publish,
        PublishStagedRevisionDecision::Replayed(_)
    ));
    assert_eq!(replayed_publish.receipt(), receipt);

    // The head never moved past what was committed before the deadline.
    assert_eq!(
        store
            .resolve_head(artifact, DEADLINE_MS)
            .expect("poll at deadline")
            .expect("head")
            .revision,
        2
    );
}

/// Retention coordination with GC is mark-only: the expired artifact's
/// blobs are still referenced by committed revision rows, so the explicit
/// orphan GC collects nothing, deletes nothing, and the bytes survive.
/// Expiry and its reversal survive restarts; extending the bound makes
/// expired data readable again; a zero-length bound expires right after
/// the anchor instant.
#[test]
fn gc_never_collects_expired_references_and_policy_survives_restart() {
    let directory = TestStoreDir::new("retention-gc-restart");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let artifact = artifact_spec(0).artifact_id;
    store.create_artifact(artifact_spec(0)).expect("create");
    let payload = bytes(0x75, 128);
    store
        .put_revision(put(artifact, 0, &payload))
        .expect("put revision");
    store
        .set_retention(set_retention(artifact, RETENTION_MS))
        .expect("set retention");

    let read_error = store
        .get_revision(artifact, 1, DEADLINE_MS + 1)
        .expect_err("expired read");
    assert_expired(&read_error, artifact, DEADLINE_MS);

    // GC runs explicitly and finds only orphans — the expired artifact's
    // blob is referenced, hence the collected set is empty.
    let decision = store
        .collect_orphan_blobs(CollectOrphanBlobsRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x95; 16]),
            collected_at_ms: DEADLINE_MS + 2,
        })
        .expect("gc run");
    assert!(matches!(decision, CollectOrphanBlobsDecision::Collected(_)));
    assert!(decision.receipt().collected_digests.is_empty());
    assert_eq!(decision.receipt().scanned_blob_count, 1);
    assert!(
        directory
            .artifact_blob(ContentDigest::of_bytes(&payload))
            .is_file(),
        "GC must not delete an expired-but-referenced blob"
    );

    // Restart: policy and expiry behaviour persist.
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    assert_eq!(
        reopened
            .inspect_retention(artifact)
            .expect("inspect")
            .retention_ms,
        Some(RETENTION_MS)
    );
    let after_restart = reopened
        .get_revision(artifact, 1, DEADLINE_MS + 1)
        .expect_err("expiry persists across restart");
    assert_expired(&after_restart, artifact, DEADLINE_MS);

    // Expiry is reversible: extending the bound re-opens readability at
    // the previously expired observation time.
    reopened
        .set_retention(set_retention(artifact, RETENTION_MS * 2))
        .expect("extend after expiry");
    assert_eq!(
        reopened
            .get_revision(artifact, 1, DEADLINE_MS + 1)
            .expect("readable again after extension"),
        payload
    );

    // A zero-length bound expires immediately after the anchor: readable
    // exactly at `created_at_ms`, expired one millisecond later.
    let zero = artifact_spec(4).artifact_id;
    reopened
        .create_artifact(artifact_spec(4))
        .expect("create zero");
    let zero_payload = bytes(0x76, 64);
    reopened
        .put_revision(put(zero, 0, &zero_payload))
        .expect("put zero-bound artifact");
    reopened
        .set_retention(set_retention(zero, 0))
        .expect("set zero bound");
    assert!(
        reopened.get_revision(zero, 1, CREATED_AT_MS_ZERO).is_ok(),
        "readable exactly at the anchor instant"
    );
    let zero_error = reopened
        .get_revision(zero, 1, CREATED_AT_MS_ZERO + 1)
        .expect_err("expired one ms past the anchor");
    assert_expired(&zero_error, zero, CREATED_AT_MS_ZERO);
}

/// The retention gate never masks metadata-plane contracts: artifact
/// identity reuse under a different idempotency key stays a typed
/// conflict, and an unexpired retained artifact still resolves its head.
#[test]
fn retention_does_not_mask_create_contract() {
    let directory = TestStoreDir::new("retention-conflict");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let spec = artifact_spec(0);
    store.create_artifact(spec.clone()).expect("create");
    store
        .set_retention(set_retention(spec.artifact_id, RETENTION_MS))
        .expect("set retention");

    let mut clashing = spec.clone();
    clashing.idempotency_key = IdempotencyKey::from_bytes([0x97; 16]);
    assert!(matches!(
        store.create_artifact(clashing),
        Err(ArtifactError::IdempotencyConflict)
    ));
    assert!(store.resolve_head(spec.artifact_id, DEADLINE_MS).is_ok());
}
