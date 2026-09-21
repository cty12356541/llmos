//! Ecosystem selector half integration tests (W36-P7, schema v5): the
//! pinned-handle G4 rules carried to ecosystem entities, the typed
//! negative matrix (unknown entity / unregistered kind / stale
//! generation / corrupt rows), the idempotency matrix, the restart
//! durability path, and the explicit freshness fence face.
//!
//! The in-file [`TestSource`] mirrors the W31-F scheduler tests'
//! in-file `AdmissionConsult` impls: the trait boundary keeps the real
//! authorities out of this crate's public face; the real-surface
//! adapters (artifact source under the `artifact-source` feature,
//! application source in `nlos-application`) carry their own tests.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_plan::{
    EcosystemEntityKind, EcosystemEntityState, EcosystemResolutionDecision,
    EcosystemResolutionError, EcosystemSelector, EcosystemSelectorSource, EcosystemSourceLookup,
    GenerationExpectation, PlanStoreError, ResolveEcosystemRequest, SqlitePlanAuthority,
};
use nlos_types::{ArtifactId, IdempotencyKey, PackageId, ReceiptId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(std::path::PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-plan-ecosystem-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const ALL_KINDS: &[EcosystemEntityKind] = &[
    EcosystemEntityKind::Application,
    EcosystemEntityKind::Artifact,
];

#[derive(Debug, PartialEq)]
struct LookupFailed;

impl std::fmt::Display for LookupFailed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("test source lookup failed")
    }
}

impl std::error::Error for LookupFailed {}

/// In-file generation-carrying source: entities advance their generation
/// under test control, so the pin-once / stale-fence / freshness faces
/// are exercised against a source that observably moves.
struct TestSource {
    kinds: &'static [EcosystemEntityKind],
    entities: HashMap<[u8; 16], (u64, [u8; 32])>,
    fail: bool,
}

impl TestSource {
    fn new(kinds: &'static [EcosystemEntityKind]) -> Self {
        Self {
            kinds,
            entities: HashMap::new(),
            fail: false,
        }
    }

    fn put(&mut self, entity: [u8; 16], generation: u64, digest: [u8; 32]) {
        self.entities.insert(entity, (generation, digest));
    }

    fn advance(&mut self, entity: [u8; 16]) {
        let entry = self
            .entities
            .get_mut(&entity)
            .expect("entity under advancement exists");
        let next = entry.0 + 1;
        *entry = (next, [next.to_be_bytes()[7]; 32]);
    }
}

impl EcosystemSelectorSource for TestSource {
    type Error = LookupFailed;

    fn kinds(&self) -> &'static [EcosystemEntityKind] {
        self.kinds
    }

    fn lookup(&self, selector: &EcosystemSelector) -> Result<EcosystemSourceLookup, Self::Error> {
        if self.fail {
            return Err(LookupFailed);
        }
        match self.entities.get(&selector.entity_id()) {
            Some((generation, digest)) => Ok(EcosystemSourceLookup::Found(EcosystemEntityState {
                generation: *generation,
                content_digest: *digest,
            })),
            None => Ok(EcosystemSourceLookup::NotFound),
        }
    }
}

fn app_selector(package: u8, expectation: GenerationExpectation) -> EcosystemSelector {
    EcosystemSelector::Application {
        package_id: PackageId::from_bytes([package; 16]),
        expectation,
    }
}

fn art_selector(artifact: u8, expectation: GenerationExpectation) -> EcosystemSelector {
    EcosystemSelector::Artifact {
        artifact_id: ArtifactId::from_bytes([artifact; 16]),
        expectation,
    }
}

fn resolve_request(selector: EcosystemSelector, key: u8) -> ResolveEcosystemRequest {
    ResolveEcosystemRequest {
        selector,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        resolved_at_ms: 7_000,
    }
}

fn user_version(path: &std::path::Path) -> i64 {
    let connection = Connection::open(path).expect("raw reader");
    connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("read user_version")
}

fn receipt_count(path: &std::path::Path) -> i64 {
    let connection = Connection::open(path).expect("raw counter");
    connection
        .query_row(
            "SELECT COUNT(*) FROM ecosystem_resolution_receipts",
            [],
            |row| row.get(0),
        )
        .expect("count receipts")
}

fn assert_integrity(path: &std::path::Path) {
    let connection = Connection::open(path).expect("integrity reader");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok");
}

/// `Current` pins exactly once and the receipt never floats: after the
/// source advances, a replay of the same key answers from the original
/// receipt, while a fresh key pins the new generation — both receipts
/// coexist, each inspectable byte-equal.
#[test]
fn ecosystem_current_selector_pins_once_and_receipt_never_floats() {
    let root = Root::new("pins-once");
    let mut source = TestSource::new(ALL_KINDS);
    let package = [0x11; 16];
    source.put(package, 3, [0x33; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    let original = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0x61),
        )
        .expect("resolve current")
        .handle();
    assert_eq!(original.generation, 3);
    assert_eq!(original.entity_id, package);
    assert_eq!(original.kind, EcosystemEntityKind::Application);
    assert_eq!(original.content_digest, [0x33; 32]);

    source.advance(package);
    let replay = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0x61),
        )
        .expect("replay same key after advance");
    assert!(matches!(replay, EcosystemResolutionDecision::Replayed(_)));
    assert_eq!(replay.handle(), original);

    let fresh = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0x62),
        )
        .expect("fresh key pins the new generation")
        .handle();
    assert_eq!(fresh.generation, 4);
    assert_eq!(
        authority
            .inspect_ecosystem_resolution(original.resolution_id)
            .expect("inspect original"),
        Some(original)
    );
    assert_eq!(
        authority
            .inspect_ecosystem_resolution(fresh.resolution_id)
            .expect("inspect fresh"),
        Some(fresh)
    );
    assert_eq!(receipt_count(&root.0), 2);
}

/// `At` resolves against the exact expected generation; after the source
/// advances, both a stale-behind and an ahead-of-head expectation fail
/// the typed stale fence, and a zero expectation is a typed request
/// error — never a silent resolve.
#[test]
fn ecosystem_at_expectation_resolves_and_stale_generation_fences_typed() {
    let root = Root::new("at-fence");
    let mut source = TestSource::new(ALL_KINDS);
    let artifact = [0x22; 16];
    source.put(artifact, 5, [0x55; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    let exact = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::At(5)), 0x71),
        )
        .expect("resolve exact generation")
        .handle();
    assert_eq!(exact.generation, 5);

    source.advance(artifact);
    assert!(matches!(
        authority.resolve_ecosystem_selector(&source, resolve_request(
            art_selector(0x22, GenerationExpectation::At(5)),
            0x72,
        )),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::StaleEcosystemGeneration {
                kind: EcosystemEntityKind::Artifact,
                entity_id,
                expected: 5,
                current: 6,
            }
        )) if entity_id == artifact
    ));
    assert!(matches!(
        authority.resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::At(9)), 0x73,)
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::StaleEcosystemGeneration {
                expected: 9,
                current: 6,
                ..
            }
        ))
    ));
    assert!(matches!(
        authority.resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::At(0)), 0x74,)
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::InvalidRequest { .. }
        ))
    ));
    assert_eq!(receipt_count(&root.0), 1);
}

/// An entity id unknown to the source is a typed miss, never resolved
/// against nothing.
#[test]
fn ecosystem_unknown_entity_fails_typed_notfound() {
    let root = Root::new("not-found");
    let source = TestSource::new(ALL_KINDS);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    assert!(matches!(
        authority.resolve_ecosystem_selector(&source, resolve_request(
            app_selector(0xee, GenerationExpectation::Current),
            0x81,
        )),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemEntityNotFound {
                kind: EcosystemEntityKind::Application,
                entity_id,
            }
        )) if entity_id == [0xee; 16]
    ));
    assert_eq!(receipt_count(&root.0), 0);
}

/// A selector whose kind the source does not declare fails typed
/// unavailable — fail-closed, no source consult, no durable row.
#[test]
fn ecosystem_kind_without_registered_source_fails_typed_unavailable() {
    let root = Root::new("no-source");
    let mut source = TestSource::new(&[EcosystemEntityKind::Artifact]);
    let package = [0x11; 16];
    source.put(package, 2, [0x22; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    assert!(matches!(
        authority.resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0x91,)
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemSourceUnavailable {
                kind: EcosystemEntityKind::Application,
            }
        ))
    ));
    assert_eq!(receipt_count(&root.0), 0);
    source.fail = true;
    assert!(matches!(
        authority.resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0x92,)
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemSourceUnavailable { .. }
        ))
    ));
}

/// A failed source lookup propagates unchanged and writes nothing
/// durable; the same key resolves freshly once the source recovers.
#[test]
fn ecosystem_source_failure_propagates_and_writes_nothing_durable() {
    let root = Root::new("source-failure");
    let mut source = TestSource::new(ALL_KINDS);
    let package = [0x11; 16];
    source.put(package, 1, [0x11; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    source.fail = true;
    assert!(matches!(
        authority.resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xa1,)
        ),
        Err(EcosystemResolutionError::Source(LookupFailed))
    ));
    assert_eq!(receipt_count(&root.0), 0);

    source.fail = false;
    let resolved = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xa1),
        )
        .expect("resolve after recovery")
        .handle();
    assert_eq!(resolved.generation, 1);
    assert_eq!(receipt_count(&root.0), 1);
}

/// The idempotency matrix: byte-equal replays (and the `At` form of the
/// same pinned generation) answer from the durable receipt; rebinding
/// the key to another entity, observation time, or generation target is
/// a typed conflict.
#[test]
fn ecosystem_idempotent_replay_answers_original_and_rebind_conflicts() {
    let root = Root::new("idem");
    let mut source = TestSource::new(ALL_KINDS);
    let package = [0x11; 16];
    source.put(package, 4, [0x44; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    let original = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xb1),
        )
        .expect("resolve")
        .handle();
    let replay = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xb1),
        )
        .expect("replay same bytes");
    assert!(matches!(replay, EcosystemResolutionDecision::Replayed(_)));
    assert_eq!(replay.handle(), original);
    let replay_at_form = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(
                app_selector(0x11, GenerationExpectation::At(original.generation)),
                0xb1,
            ),
        )
        .expect("replay targeting the same pinned generation");
    assert!(matches!(
        replay_at_form,
        EcosystemResolutionDecision::Replayed(_)
    ));

    let mut rebound = resolve_request(app_selector(0x12, GenerationExpectation::Current), 0xb1);
    assert!(matches!(
        authority.resolve_ecosystem_selector(&source, rebound),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::IdempotencyConflict
        ))
    ));
    rebound = resolve_request(
        app_selector(0x11, GenerationExpectation::At(original.generation + 1)),
        0xb1,
    );
    assert!(matches!(
        authority.resolve_ecosystem_selector(&source, rebound),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::IdempotencyConflict
        ))
    ));
    let mut later_time = resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xb1);
    later_time.resolved_at_ms = 7_001;
    assert!(matches!(
        authority.resolve_ecosystem_selector(&source, later_time),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::IdempotencyConflict
        ))
    ));
    assert_eq!(receipt_count(&root.0), 1);
}

/// Receipts are durable across restart, the pinned readback survives
/// reopen byte-equal, the schema head is v5, and an artificially
/// down-stamped database re-migrates idempotently to the head.
#[test]
fn ecosystem_receipts_survive_restart_and_reopen_stays_at_head() {
    let root = Root::new("restart");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let mut source = TestSource::new(ALL_KINDS);
    let artifact = [0x22; 16];
    source.put(artifact, 2, [0x22; 32]);
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    assert_eq!(user_version(&db_path), 6);
    let handle = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::Current), 0xc1),
        )
        .expect("resolve")
        .handle();
    drop(authority);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen");
    assert_eq!(user_version(&db_path), 6);
    assert_eq!(
        reopened
            .inspect_ecosystem_resolution(handle.resolution_id)
            .expect("inspect after restart")
            .expect("receipt survives"),
        handle
    );
    assert!(matches!(
        reopened.inspect_ecosystem_resolution(ReceiptId::from_bytes([0xee; 16])),
        Ok(None)
    ));
    assert!(matches!(
        reopened.verify_ecosystem_resolution_current(&source, ReceiptId::from_bytes([0xee; 16])),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemResolutionNotFound(_)
        ))
    ));
    drop(reopened);

    let raw = Connection::open(&db_path).expect("raw writer");
    raw.pragma_update(None, "user_version", 4)
        .expect("stamp v4");
    drop(raw);
    let remigrated = SqlitePlanAuthority::open(&db_path).expect("idempotent re-migration");
    assert_eq!(user_version(&db_path), 6);
    assert_eq!(
        remigrated
            .inspect_ecosystem_resolution(handle.resolution_id)
            .expect("inspect after re-migration")
            .expect("receipt survives"),
        handle
    );
    assert_integrity(&db_path);
}

/// One multi-kind source resolves both entity kinds independently:
/// distinct ids, distinct receipts, kinds round-trip through storage.
#[test]
fn ecosystem_two_kinds_resolve_through_one_multi_kind_source() {
    let root = Root::new("two-kinds");
    let mut source = TestSource::new(ALL_KINDS);
    source.put([0x11; 16], 7, [0x17; 32]);
    source.put([0x22; 16], 9, [0x19; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");

    let application = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xd1),
        )
        .expect("application kind resolves")
        .handle();
    let artifact = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::Current), 0xd2),
        )
        .expect("artifact kind resolves")
        .handle();
    assert_eq!(application.kind, EcosystemEntityKind::Application);
    assert_eq!(artifact.kind, EcosystemEntityKind::Artifact);
    assert_eq!(application.generation, 7);
    assert_eq!(artifact.generation, 9);
    assert_ne!(application.resolution_id, artifact.resolution_id);
}

/// The explicit freshness fence: verify passes while the entity is still
/// at the pinned generation, fails typed stale after an advance (either
/// direction), and fails typed not-found once the entity disappears.
#[test]
fn ecosystem_verify_current_face_detects_stale_generation() {
    let root = Root::new("freshness");
    let mut source = TestSource::new(ALL_KINDS);
    let package = [0x11; 16];
    source.put(package, 3, [0x33; 32]);
    let authority = SqlitePlanAuthority::open(&root.0).expect("open authority");
    let handle = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(app_selector(0x11, GenerationExpectation::Current), 0xe1),
        )
        .expect("resolve")
        .handle();

    assert_eq!(
        authority
            .verify_ecosystem_resolution_current(&source, handle.resolution_id)
            .expect("fresh receipt verifies"),
        handle
    );

    source.advance(package);
    assert!(matches!(
        authority.verify_ecosystem_resolution_current(&source, handle.resolution_id),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::StaleEcosystemGeneration {
                expected: 3,
                current: 4,
                ..
            }
        ))
    ));

    source.entities.remove(&package);
    assert!(matches!(
        authority.verify_ecosystem_resolution_current(&source, handle.resolution_id),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemEntityNotFound { .. }
        ))
    ));
}

/// Stored-row negative faces: an entity kind outside the closed enum is
/// refused typed on readback (the decode guard — the v5 schema
/// deliberately carries no IN-list CHECK so this face stays falsifiable),
/// a row whose id does not re-derive from its fields fails closed, and
/// the write-once triggers abort raw UPDATE/DELETE.
#[test]
fn ecosystem_unknown_kind_and_tampered_rows_fail_typed_closed() {
    let root = Root::new("tamper");
    let db_path = root.0.join("plan.sqlite3");
    std::fs::create_dir_all(&root.0).expect("create db directory");
    let mut source = TestSource::new(ALL_KINDS);
    source.put([0x22; 16], 1, [0x11; 32]);
    let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
    let handle = authority
        .resolve_ecosystem_selector(
            &source,
            resolve_request(art_selector(0x22, GenerationExpectation::Current), 0xf1),
        )
        .expect("resolve")
        .handle();
    drop(authority);

    let raw = Connection::open(&db_path).expect("raw writer");
    let unknown_kind_id: [u8; 16] = [0x99; 16];
    raw.execute(
        "INSERT INTO ecosystem_resolution_receipts (
            resolution_id, idempotency_key, entity_kind, entity_id,
            generation, content_digest, resolved_at_ms
         ) VALUES (?1, ?2, 99, ?3, 1, ?4, 1)",
        rusqlite::params![
            unknown_kind_id.as_slice(),
            [0x98_u8; 16].as_slice(),
            [0x22_u8; 16].as_slice(),
            [0_u8; 32].as_slice(),
        ],
    )
    .expect("insert unknown-kind row (no IN-list CHECK by design)");
    let non_deriving_id: [u8; 16] = [0x97; 16];
    raw.execute(
        "INSERT INTO ecosystem_resolution_receipts (
            resolution_id, idempotency_key, entity_kind, entity_id,
            generation, content_digest, resolved_at_ms
         ) VALUES (?1, ?2, 1, ?3, 1, ?4, 1)",
        rusqlite::params![
            non_deriving_id.as_slice(),
            [0x96_u8; 16].as_slice(),
            [0x11_u8; 16].as_slice(),
            [0_u8; 32].as_slice(),
        ],
    )
    .expect("insert non-deriving row");
    assert!(
        raw.execute(
            "UPDATE ecosystem_resolution_receipts SET generation = 9",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM ecosystem_resolution_receipts", [])
            .is_err()
    );
    drop(raw);

    let reopened = SqlitePlanAuthority::open(&db_path).expect("reopen");
    assert!(matches!(
        reopened.inspect_ecosystem_resolution(ReceiptId::from_bytes(unknown_kind_id)),
        Err(PlanStoreError::EcosystemKindUnknown(99))
    ));
    assert!(matches!(
        reopened.inspect_ecosystem_resolution(ReceiptId::from_bytes(non_deriving_id)),
        Err(PlanStoreError::CorruptRecord(_))
    ));
    assert_eq!(
        reopened
            .inspect_ecosystem_resolution(handle.resolution_id)
            .expect("legitimate receipt still reads"),
        Some(handle)
    );
    assert_integrity(&db_path);
}

/// Real-surface adapter round-trip under the `artifact-source` feature:
/// a live `ArtifactStore` head (revisions advanced by real `put_revision`
/// CAS writes) resolves through the plan authority, pins its content
/// digest, and the advance fences stale generations on both the resolve
/// and verify faces.
#[cfg(feature = "artifact-source")]
mod artifact_adapter {
    use nlos_artifact::{
        ArtifactStore, ContentDigest, CreateArtifactSpec, ProvenanceSourceTriple,
        PutRevisionRequest,
    };
    use nlos_plan::{
        ArtifactSelectorSource, EcosystemEntityKind, GenerationExpectation, PlanStoreError,
        ResolveEcosystemRequest, SqlitePlanAuthority,
    };
    use nlos_types::{ArtifactId, IdempotencyKey};

    use crate::{Root, art_selector, resolve_request};

    fn put(store: &ArtifactStore, artifact: ArtifactId, expected_head: u64, bytes: &[u8], key: u8) {
        store
            .put_revision(PutRevisionRequest {
                artifact_id: artifact,
                expected_head_revision: expected_head,
                bytes,
                created_at_ms: 1_000,
                provenance: ProvenanceSourceTriple {
                    source_a: [key; 16],
                    source_b: [key; 16],
                    source_digest: ContentDigest::from_bytes([key; 32]),
                },
            })
            .expect("put revision");
    }

    #[test]
    fn artifact_source_resolves_real_head_and_fences_on_advance() {
        let root = Root::new("artifact-adapter");
        let db_path = root.0.join("plan.sqlite3");
        std::fs::create_dir_all(&root.0).expect("create db directory");
        let store = ArtifactStore::open(&root.0).expect("open artifact store");
        let artifact = ArtifactId::from_bytes([0x44; 16]);
        store
            .create_artifact(CreateArtifactSpec {
                artifact_id: artifact,
                idempotency_key: IdempotencyKey::from_bytes([0x45; 16]),
                content_type: "application/octet-stream".to_owned(),
                application_id: None,
                owner: None,
                created_at_ms: 1_000,
            })
            .expect("create artifact");
        put(&store, artifact, 0, b"v1", 0x46);
        put(&store, artifact, 1, b"v2", 0x47);

        let authority = SqlitePlanAuthority::open(&db_path).expect("open authority");
        let source = ArtifactSelectorSource::new(&store, 2_000);
        let handle = authority
            .resolve_ecosystem_selector(
                &source,
                resolve_request(art_selector(0x44, GenerationExpectation::At(2)), 0x51),
            )
            .expect("resolve real artifact head")
            .handle();
        assert_eq!(handle.kind, EcosystemEntityKind::Artifact);
        assert_eq!(handle.generation, 2);
        assert_eq!(
            handle.content_digest,
            ContentDigest::of_bytes(b"v2").into_bytes()
        );
        assert_eq!(
            authority
                .verify_ecosystem_resolution_current(&source, handle.resolution_id)
                .expect("fresh head verifies"),
            handle
        );

        put(&store, artifact, 2, b"v3", 0x48);
        assert!(matches!(
            authority.resolve_ecosystem_selector(
                &source,
                resolve_request(art_selector(0x44, GenerationExpectation::At(2)), 0x52,)
            ),
            Err(nlos_plan::EcosystemResolutionError::Plan(
                PlanStoreError::StaleEcosystemGeneration {
                    expected: 2,
                    current: 3,
                    ..
                }
            ))
        ));
        assert!(matches!(
            authority.verify_ecosystem_resolution_current(&source, handle.resolution_id),
            Err(nlos_plan::EcosystemResolutionError::Plan(
                PlanStoreError::StaleEcosystemGeneration {
                    expected: 2,
                    current: 3,
                    ..
                }
            ))
        ));

        let unknown = ResolveEcosystemRequest {
            selector: art_selector(0xee, GenerationExpectation::Current),
            idempotency_key: IdempotencyKey::from_bytes([0x53; 16]),
            resolved_at_ms: 7_000,
        };
        assert!(matches!(
            authority.resolve_ecosystem_selector(&source, unknown),
            Err(nlos_plan::EcosystemResolutionError::Plan(
                PlanStoreError::EcosystemEntityNotFound { .. }
            ))
        ));
    }
}
