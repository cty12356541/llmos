//! B-ARTIFACT-001: immutable revisions and mutable-head CAS
//! (`[ART-VERSION-001]` spirit).
//!
//! Decision order inside the commit transaction (spec-interpretation
//! decision, documented in the evidence draft):
//!
//! 1. same derived revision + same digest -> `Replayed` (idempotent);
//! 2. head mismatch -> typed `HeadConflict` (a competing put won, or the
//!    expectation names a nonexistent future head);
//! 3. occupied slot under a matching head -> fail-closed `RevisionConflict`
//!    (metadata-inconsistency guard; unreachable through the public API).

mod support;

use nlos_artifact::{ArtifactError, ArtifactStore, PutRevisionDecision};
use support::{TestStoreDir, artifact_id, artifact_spec, bytes, put};

#[test]
fn same_revision_same_bytes_replays_idempotently() {
    let directory = TestStoreDir::new("replay");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x10)).expect("create");

    let payload = bytes(0x31, 512);
    let committed = match store
        .put_revision(put(artifact_id(0x10), 0, &payload))
        .expect("first put")
    {
        PutRevisionDecision::Committed(record) => record,
        PutRevisionDecision::Replayed(_) => panic!("first put must commit"),
    };

    // Retry after a simulated lost response: identical request replays.
    let replayed = store
        .put_revision(put(artifact_id(0x10), 0, &payload))
        .expect("replay put");
    match replayed {
        PutRevisionDecision::Replayed(record) => assert_eq!(record, committed),
        PutRevisionDecision::Committed(_) => panic!("replay must not recommit"),
    }
    assert_eq!(
        store
            .resolve_head(artifact_id(0x10))
            .expect("head")
            .expect("head")
            .revision,
        1,
        "replay must not advance the head"
    );
    assert_eq!(
        store.list_revisions(artifact_id(0x10)).expect("list").len(),
        1
    );
}

#[test]
fn same_revision_different_bytes_fails_closed() {
    let directory = TestStoreDir::new("immutable");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x11)).expect("create");

    let original = bytes(0x41, 512);
    store
        .put_revision(put(artifact_id(0x11), 0, &original))
        .expect("first put");

    // Same derived revision id (expected_head 0 -> revision 1), different
    // content: fail closed with a typed error, never overwrite.
    let error = store
        .put_revision(put(artifact_id(0x11), 0, &bytes(0x42, 512)))
        .expect_err("conflicting revision content must fail");
    assert!(
        matches!(
            error,
            ArtifactError::HeadConflict { .. } | ArtifactError::RevisionConflict { .. }
        ),
        "expected a typed conflict, got {error}"
    );

    // The immutable revision is untouched.
    assert_eq!(
        store
            .get_revision(artifact_id(0x11), 1)
            .expect("revision 1"),
        original
    );
    assert_eq!(
        store
            .resolve_head(artifact_id(0x11))
            .expect("head")
            .expect("head")
            .revision,
        1
    );

    // The DDL additionally enforces immutability below the API: direct
    // UPDATE/DELETE on committed revisions is rejected by triggers.
    let raw = rusqlite::Connection::open(directory.root().join("metadata.db")).expect("raw open");
    assert!(
        raw.execute("UPDATE artifact_revisions SET digest = digest", [])
            .is_err(),
        "revision UPDATE must be rejected by trigger"
    );
    assert!(
        raw.execute("DELETE FROM artifact_revisions", []).is_err(),
        "revision DELETE must be rejected by trigger"
    );
}

#[test]
fn head_cas_two_competing_puts_exactly_one_wins() {
    let directory = TestStoreDir::new("head-cas");
    let store_a = ArtifactStore::open(directory.root()).expect("open A");
    // A second store on the same root models a competing writer.
    let store_b = ArtifactStore::open(directory.root()).expect("open B");
    store_a
        .create_artifact(artifact_spec(0x12))
        .expect("create");

    // Both competitors observed head = 0.
    let winner = store_a
        .put_revision(put(artifact_id(0x12), 0, &bytes(0x51, 256)))
        .expect("winner put");
    assert!(matches!(winner, PutRevisionDecision::Committed(_)));

    // The loser's stale head expectation fails closed with HeadConflict.
    let loser = store_b
        .put_revision(put(artifact_id(0x12), 0, &bytes(0x52, 256)))
        .expect_err("stale head must lose the CAS");
    assert!(
        matches!(
            loser,
            ArtifactError::HeadConflict {
                expected: 0,
                current: 1
            }
        ),
        "expected HeadConflict, got {loser}"
    );

    // A future (gap) expectation is likewise rejected.
    let gap = store_b
        .put_revision(put(artifact_id(0x12), 5, &bytes(0x53, 256)))
        .expect_err("future head expectation must fail");
    assert!(
        matches!(
            gap,
            ArtifactError::HeadConflict {
                expected: 5,
                current: 1
            }
        ),
        "expected HeadConflict, got {gap}"
    );

    // The winner's revision is the only committed content; the loser
    // re-resolves and can then advance the head legitimately.
    assert_eq!(
        store_b
            .get_revision(artifact_id(0x12), 1)
            .expect("revision 1"),
        bytes(0x51, 256)
    );
    let head = store_b
        .resolve_head(artifact_id(0x12))
        .expect("resolve")
        .expect("head");
    assert_eq!(head.revision, 1);
    let second = store_b
        .put_revision(put(artifact_id(0x12), head.revision, &bytes(0x54, 256)))
        .expect("loser retries at the resolved head");
    assert!(matches!(
        second,
        PutRevisionDecision::Committed(ref record) if record.revision == 2
    ));
    assert_eq!(
        store_a
            .resolve_head(artifact_id(0x12))
            .expect("resolve")
            .expect("head")
            .revision,
        2
    );
}

#[test]
fn put_on_unknown_artifact_is_typed() {
    let directory = TestStoreDir::new("put-notfound");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let error = store
        .put_revision(put(artifact_id(0x7e), 0, &bytes(0x61, 16)))
        .expect_err("put on unknown artifact must fail");
    assert!(matches!(error, ArtifactError::ArtifactNotFound(_)));
    assert!(matches!(
        store.list_revisions(artifact_id(0x7e)),
        Err(ArtifactError::ArtifactNotFound(_))
    ));
}
