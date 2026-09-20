//! W28-E auto orphan-GC trigger: caller-driven periodic/threshold tick
//! over the unchanged explicit GC core.
//!
//! Every test pins one contract of the tick:
//!
//! - the GC deletion core is untouched — the tick shares
//!   `collect_orphan_blobs`, so the conservative reference set and the
//!   delete-then-receipt order are inherited, and the false-orphan
//!   regression here exercises the trigger path end to end;
//! - the trigger owns an independent derived idempotency-key space (one
//!   key per durable pass index), never aliasing caller keys;
//! - `Disabled` is a full bypass: no scan, no pass, no key, no counters;
//! - the health counters are durable and typed.

use std::fs;

use nlos_artifact::{
    ArtifactStore, AutoGcSkipReason, AutoGcTickDecision, AutoOrphanGcPolicy,
    CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, ContentDigest, StageRevisionRequest,
    TickAutoGcRequest,
};
use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskId};
use support::{READ_NOW_MS, TestStoreDir, artifact_id, artifact_spec, bytes, put};

mod support;

fn tick(now_ms: u64, policy: AutoOrphanGcPolicy) -> TickAutoGcRequest {
    TickAutoGcRequest { now_ms, policy }
}

fn interval(ms: u64) -> AutoOrphanGcPolicy {
    AutoOrphanGcPolicy::Enabled {
        interval_ms: Some(ms),
        orphan_threshold: None,
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
            task_id: TaskId::from_bytes([0x87; 16]),
            permit_id: CommitPermitId::from_bytes([0x88; 16]),
            write_set_root: ContentDigest::of_bytes(b"write-set"),
            idempotency_key: IdempotencyKey::from_bytes([key; 16]),
            created_at_ms: 6_000,
        })
        .expect("stage");
}

fn gc_receipt_count(root: &std::path::Path) -> i64 {
    let raw = rusqlite::Connection::open(root.join("metadata.db")).expect("raw open");
    raw.query_row("SELECT COUNT(*) FROM artifact_gc_receipts", [], |row| {
        row.get(0)
    })
    .expect("count gc receipts")
}

/// Rewinds the mutable auto-GC state row to its pristine zero state via
/// raw SQL, reconstructing the exact crash window "GC receipt committed,
/// state advance lost" for the pass that already ran.
fn rewind_auto_gc_state(root: &std::path::Path) {
    let raw = rusqlite::Connection::open(root.join("metadata.db")).expect("raw open");
    raw.execute(
        "UPDATE artifact_auto_gc_state
         SET passes_completed = 0, orphans_collected_total = 0, failure_count = 0,
             last_pass_at_ms = NULL, last_failure_at_ms = NULL
         WHERE singleton = 0",
        [],
    )
    .expect("rewind auto gc state");
}

/// `Disabled` is a full bypass (`AutoOrphanGc` precedent): the tick touches
/// nothing — no scan, no pass, no receipt, no counter — and a later
/// enabled tick still collects, so no key space was consumed.
#[test]
fn tick_disabled_is_full_bypass() {
    let directory = TestStoreDir::new("auto-gc-disabled");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let orphan = plant_orphan(&directory, 0x71, 64);

    let decision = store
        .tick_auto_gc(tick(1_000, AutoOrphanGcPolicy::Disabled))
        .expect("disabled tick");
    assert_eq!(
        decision,
        AutoGcTickDecision::Skipped(AutoGcSkipReason::Disabled)
    );
    assert!(directory.artifact_blob(orphan).is_file(), "no pass may run");
    assert_eq!(gc_receipt_count(directory.root()), 0, "no key consumed");
    assert_eq!(
        store.inspect_auto_gc_health().expect("health"),
        nlos_artifact::AutoGcHealth {
            passes_completed: 0,
            orphans_collected_total: 0,
            failure_count: 0,
            last_pass_at_ms: None,
            last_failure_at_ms: None,
        }
    );

    let enabled = store
        .tick_auto_gc(tick(1_100, interval(500)))
        .expect("enabled tick after bypass");
    assert!(matches!(enabled, AutoGcTickDecision::Collected(_)));
    assert_eq!(
        enabled.receipt().expect("receipt").collected_digests,
        vec![orphan]
    );
}

/// Periodic trigger: the first eligible tick runs one pass; an early
/// second tick is gated (double-fire = single pass); at the interval
/// boundary a fresh pass runs under a fresh key; the health counters
/// accumulate and survive restart.
#[test]
fn tick_interval_double_fire_runs_single_pass() {
    let directory = TestStoreDir::new("auto-gc-interval");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let orphan_a = plant_orphan(&directory, 0x72, 64);
    let orphan_b = plant_orphan(&directory, 0x73, 64);

    let first = store
        .tick_auto_gc(tick(1_000, interval(500)))
        .expect("first tick");
    let AutoGcTickDecision::Collected(receipt) = first else {
        panic!("first tick must run a fresh pass, got {first:?}");
    };
    let mut expected = vec![orphan_a, orphan_b];
    expected.sort();
    assert_eq!(receipt.collected_digests, expected);
    assert_eq!(receipt.created_at_ms, 1_000);

    // Double-fire inside the interval: gated, nothing re-runs, and a new
    // orphan planted after the pass survives the early tick.
    let orphan_c = plant_orphan(&directory, 0x74, 64);
    let early = store
        .tick_auto_gc(tick(1_400, interval(500)))
        .expect("early tick");
    assert_eq!(
        early,
        AutoGcTickDecision::Skipped(AutoGcSkipReason::IntervalNotElapsed {
            last_pass_at_ms: 1_000,
            eligible_at_ms: 1_500,
        })
    );
    assert!(directory.artifact_blob(orphan_c).is_file());
    assert_eq!(gc_receipt_count(directory.root()), 1, "single pass so far");

    // At the boundary the next pass runs under a fresh key.
    let second = store
        .tick_auto_gc(tick(1_500, interval(500)))
        .expect("boundary tick");
    assert!(matches!(second, AutoGcTickDecision::Collected(_)));
    assert_eq!(
        second.receipt().expect("receipt").collected_digests,
        vec![orphan_c]
    );
    assert_eq!(
        gc_receipt_count(directory.root()),
        2,
        "one receipt per pass"
    );

    let health = store.inspect_auto_gc_health().expect("health");
    assert_eq!(health.passes_completed, 2);
    assert_eq!(health.orphans_collected_total, 3);
    assert_eq!(health.failure_count, 0);
    assert_eq!(health.last_pass_at_ms, Some(1_500));
    assert_eq!(health.last_failure_at_ms, None);

    // Health is durable across restart.
    drop(store);
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");
    assert_eq!(reopened.inspect_auto_gc_health().expect("health"), health);
    assert_eq!(
        reopened
            .tick_auto_gc(tick(1_599, interval(500)))
            .expect("gated after restart"),
        AutoGcTickDecision::Skipped(AutoGcSkipReason::IntervalNotElapsed {
            last_pass_at_ms: 1_500,
            eligible_at_ms: 2_000,
        })
    );
}

/// Threshold trigger without a period: the candidate count is the true
/// orphan diff (referenced revisions, live heads, and unreleased staged
/// blobs are never candidates), and only meeting the threshold fires the
/// pass — which then deletes exactly the provable orphans. This is the
/// false-orphan regression for the trigger path.
#[test]
fn tick_threshold_counts_true_orphans_and_never_fires_on_referenced_blobs() {
    let directory = TestStoreDir::new("auto-gc-threshold");
    let store = ArtifactStore::open(directory.root()).expect("open");

    // In-registry references: two committed revisions on one artifact and
    // an unreleased staged revision on the other.
    store.create_artifact(artifact_spec(0x40)).expect("create");
    let p1 = bytes(0x80, 128);
    let p2 = bytes(0x81, 128);
    store
        .put_revision(put(artifact_id(0x40), 0, &p1))
        .expect("put revision 1");
    store
        .put_revision(put(artifact_id(0x40), 1, &p2))
        .expect("put revision 2");
    store
        .create_artifact(artifact_spec(0x41))
        .expect("create B");
    let staged = bytes(0x82, 128);
    stage(&store, artifact_id(0x41), 0, &staged, 0x89);

    let orphan = plant_orphan(&directory, 0x75, 64);
    let below = store
        .tick_auto_gc(tick(
            2_000,
            AutoOrphanGcPolicy::Enabled {
                interval_ms: None,
                orphan_threshold: Some(2),
            },
        ))
        .expect("threshold tick below");
    assert_eq!(
        below,
        AutoGcTickDecision::Skipped(AutoGcSkipReason::BelowThreshold {
            orphan_candidates: 1,
            orphan_threshold: 2,
        })
    );
    assert!(directory.artifact_blob(orphan).is_file());
    assert_eq!(gc_receipt_count(directory.root()), 0);

    // Meeting the threshold fires one pass that collects exactly the two
    // provable orphans and retains every referenced blob.
    let orphan_b = plant_orphan(&directory, 0x76, 64);
    let fired = store
        .tick_auto_gc(tick(
            2_100,
            AutoOrphanGcPolicy::Enabled {
                interval_ms: None,
                orphan_threshold: Some(2),
            },
        ))
        .expect("threshold tick fires");
    let mut expected = vec![orphan, orphan_b];
    expected.sort();
    assert_eq!(
        fired.receipt().expect("receipt").collected_digests,
        expected
    );
    for payload in [&p1, &p2, &staged] {
        assert!(
            directory
                .artifact_blob(ContentDigest::of_bytes(payload))
                .is_file(),
            "referenced blob must survive the auto pass"
        );
    }
    assert_eq!(
        store
            .get_revision(artifact_id(0x40), 2, READ_NOW_MS)
            .expect("referenced revision still reads"),
        p2
    );
    let report = store.recover().expect("recover");
    assert!(report.orphan_blobs.is_empty());
    assert!(report.missing_blobs.is_empty());
    assert!(report.missing_staged_blobs.is_empty());
    assert_eq!(
        store
            .inspect_auto_gc_health()
            .expect("health")
            .passes_completed,
        1
    );
}

/// An enabled policy with neither a period nor a threshold can never
/// fire: fail-closed explicit skip, nothing scanned or consumed.
#[test]
fn tick_never_eligible_fail_closed() {
    let directory = TestStoreDir::new("auto-gc-never");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let orphan = plant_orphan(&directory, 0x77, 64);

    let decision = store
        .tick_auto_gc(tick(
            3_000,
            AutoOrphanGcPolicy::Enabled {
                interval_ms: None,
                orphan_threshold: None,
            },
        ))
        .expect("empty policy tick");
    assert_eq!(
        decision,
        AutoGcTickDecision::Skipped(AutoGcSkipReason::NeverEligible)
    );
    assert!(directory.artifact_blob(orphan).is_file());
    assert_eq!(gc_receipt_count(directory.root()), 0);
    assert_eq!(
        store
            .inspect_auto_gc_health()
            .expect("health")
            .passes_completed,
        0
    );
}

/// Crash window "receipt committed, state advance lost": the next tick
/// re-derives the same pass key, replays the durable receipt verbatim,
/// and advances the counters exactly once. A degenerate zero threshold
/// is rejected up front.
#[test]
fn tick_replays_durable_receipt_and_advances_once_after_state_loss() {
    let directory = TestStoreDir::new("auto-gc-replay-window");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let orphan = plant_orphan(&directory, 0x78, 64);

    let first = store
        .tick_auto_gc(tick(4_000, interval(100)))
        .expect("first tick");
    assert!(matches!(first, AutoGcTickDecision::Collected(_)));
    assert!(!directory.artifact_blob(orphan).exists());

    // Simulate the crash between receipt commit and state advance.
    drop(store);
    rewind_auto_gc_state(directory.root());
    let reopened = ArtifactStore::open(directory.root()).expect("reopen");

    let converged = reopened
        .tick_auto_gc(tick(4_050, interval(100)))
        .expect("converging tick");
    assert!(
        matches!(converged, AutoGcTickDecision::Replayed(ref replayed) if *replayed == *first.receipt().expect("receipt")),
        "must replay the durable receipt, got {converged:?}"
    );
    assert_eq!(gc_receipt_count(directory.root()), 1, "no second pass");

    let health = reopened.inspect_auto_gc_health().expect("health");
    assert_eq!(health.passes_completed, 1, "advanced exactly once");
    assert_eq!(health.orphans_collected_total, 1);
    assert_eq!(health.last_pass_at_ms, Some(4_050));

    // Zero threshold is a degenerate "always fire" — rejected fail-closed.
    let rejected = reopened.tick_auto_gc(tick(
        4_060,
        AutoOrphanGcPolicy::Enabled {
            interval_ms: None,
            orphan_threshold: Some(0),
        },
    ));
    assert!(matches!(
        rejected,
        Err(nlos_artifact::ArtifactError::InvalidSpec(_))
    ));
}

/// A failed pass counts in the health state (failure counter, no pass
/// credit, no window reset) and the retry reuses the same pass index, so
/// the collection converges under one receipt. The unremovable orphan is
/// a digest-named directory: a scan candidate whose removal fails.
#[test]
fn tick_failure_counts_health_and_retries_same_pass_index() {
    let directory = TestStoreDir::new("auto-gc-failure");
    let store = ArtifactStore::open(directory.root()).expect("open");
    store.create_artifact(artifact_spec(0x42)).expect("create");
    let live = bytes(0x83, 128);
    store
        .put_revision(put(artifact_id(0x42), 0, &live))
        .expect("put");

    let removable = plant_orphan(&directory, 0x79, 64);
    // An empty directory at a valid digest address: scanned as present,
    // unreferenced, but `remove_file` on a directory fails — a
    // deterministic mid-removal failure without the fault shim.
    let unremovable_digest = ContentDigest::of_bytes(b"unremovable-directory-orphan");
    let unremovable_path = directory.artifact_blob(unremovable_digest);
    fs::create_dir_all(&unremovable_path).expect("digest-named directory");

    let failed = store.tick_auto_gc(tick(5_000, interval(100)));
    let error = failed.expect_err("removal of a directory must fail the pass");
    assert!(
        matches!(error, nlos_artifact::ArtifactError::Io(_)),
        "expected an I/O failure, got {error}"
    );

    let health = store
        .inspect_auto_gc_health()
        .expect("health after failure");
    assert_eq!(health.failure_count, 1);
    assert_eq!(health.last_failure_at_ms, Some(5_000));
    assert_eq!(
        health.passes_completed, 0,
        "no pass credit for a failed run"
    );
    assert_eq!(health.last_pass_at_ms, None, "no window reset by a failure");
    assert_eq!(
        gc_receipt_count(directory.root()),
        0,
        "no receipt for the failed pass"
    );

    // Repair the unremovable orphan; the retry still sees pass index 0,
    // recomputes the diff (the removable orphan may or may not have been
    // deleted before the failure — both orders converge) and commits one
    // receipt for pass 0.
    fs::remove_dir(&unremovable_path).expect("remove directory orphan");
    let retried = store
        .tick_auto_gc(tick(5_010, interval(100)))
        .expect("retry tick");
    assert!(matches!(retried, AutoGcTickDecision::Collected(_)));
    let receipt = retried.receipt().expect("receipt");
    assert_eq!(receipt.collected_count, 1);
    assert!(!directory.artifact_blob(removable).exists());
    assert!(!unremovable_path.exists());
    assert!(
        directory
            .artifact_blob(ContentDigest::of_bytes(&live))
            .is_file(),
        "referenced blob survives the failed and retried passes"
    );
    assert_eq!(gc_receipt_count(directory.root()), 1);

    let health = store.inspect_auto_gc_health().expect("health after retry");
    assert_eq!(health.passes_completed, 1);
    assert_eq!(health.orphans_collected_total, 1);
    assert_eq!(health.failure_count, 1, "failures are never forgotten");
    assert_eq!(health.last_pass_at_ms, Some(5_010));
}

/// The trigger's derived key space never aliases caller-supplied manual
/// GC keys: manual receipts and auto receipts coexist, and a manual
/// replay is unaffected by auto passes.
#[test]
fn tick_key_space_stays_independent_of_manual_keys() {
    let directory = TestStoreDir::new("auto-gc-key-space");
    let store = ArtifactStore::open(directory.root()).expect("open");
    let orphan = plant_orphan(&directory, 0x7a, 64);

    let manual_key = IdempotencyKey::from_bytes([0x2b; 16]);
    let manual = store
        .collect_orphan_blobs(CollectOrphanBlobsRequest {
            idempotency_key: manual_key,
            collected_at_ms: 6_000,
        })
        .expect("manual gc");
    assert!(matches!(manual, CollectOrphanBlobsDecision::Collected(_)));
    assert_eq!(manual.receipt().collected_digests, vec![orphan]);
    assert!(!directory.artifact_blob(orphan).exists());

    // A later auto pass (empty diff) must neither replay the manual
    // receipt nor collide with its key.
    let auto = store
        .tick_auto_gc(tick(6_100, interval(10)))
        .expect("auto tick");
    assert!(matches!(auto, AutoGcTickDecision::Collected(_)));
    let auto_receipt = auto.receipt().expect("receipt");
    assert_eq!(auto_receipt.collected_count, 0);
    assert_ne!(auto_receipt.receipt_id, manual.receipt().receipt_id);
    assert_eq!(
        gc_receipt_count(directory.root()),
        2,
        "two distinct keys, two receipts"
    );

    let replayed = store
        .collect_orphan_blobs(CollectOrphanBlobsRequest {
            idempotency_key: manual_key,
            collected_at_ms: 6_200,
        })
        .expect("manual replay");
    assert_eq!(replayed.receipt(), manual.receipt());
}
