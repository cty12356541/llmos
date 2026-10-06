//! W50-L2 + W51-L1: the federation mechanism face over one shared file
//! directory — `CellDirectory` (cross-Cell name/service discovery),
//! `MigrationIntent` (append-only intent log, no execution), and the
//! reconciliation/audit checkpoint face (append-only per-Cell fact log,
//! registration + enumeration only).
//!
//! ADR-0019 decision 5 names these three §26.1 control-plane
//! responsibilities as the mechanism face of federation. This file pins
//! the single-directory semantics: whole-record atomic registration,
//! monotone heartbeat (refused backwards), boot supersession, the
//! discovery read faces (enumerate / by name / live window), the
//! append-only ordered intent log, and the append-only checkpoint log
//! enumerated under its owning Cell's filename prefix.
//!
//! Single-process unit surface: the *dual-CellHost, dual-process*
//! evidence of ADR-0019's precondition lives in `nlos-slice-k`
//! (`tests/dual_cell_federation.rs`, `tests/dual_cell_checkpoint.rs`);
//! this file proves the mechanism under it.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_cell::{
    CellDirectory, CellEpoch, CellFencingToken, CellIdentity, FederationError, MigrationIntent,
    MigrationObject,
};
use nlos_types::{Generation, SchedulerDomainId};

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-cell-federation-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create federation temp root");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove federation temp root: {error}"),
        }
    }
}

fn identity(byte: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([byte; 16]))
}

fn registration(byte: u8, heartbeat: u64) -> nlos_cell::CellRegistrationEntry {
    nlos_cell::CellRegistrationEntry::new(
        identity(byte),
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        std::process::id(),
        "nlos-cell federation unit fixture",
    )
    .expect("valid registration")
    .with_heartbeat(heartbeat)
}

fn intent(source: u8, target: u8, object: u8, generation: u64) -> MigrationIntent {
    MigrationIntent::new(
        identity(source),
        identity(target),
        MigrationObject::from_bytes([object; 16]),
        Generation::new(NonZeroU64::new(generation).expect("nonzero generation")),
        "w50-l2 unit fixture intent",
    )
    .expect("valid intent")
}

fn fact(digest: &str) -> nlos_cell::CheckpointFact {
    nlos_cell::CheckpointFact::new(
        Generation::INITIAL,
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        digest,
    )
    .expect("valid checkpoint fact")
}

fn advanced_fact(digest: &str) -> nlos_cell::CheckpointFact {
    nlos_cell::CheckpointFact::new(
        Generation::INITIAL.checked_next().expect("boot 2"),
        CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        CellFencingToken::INITIAL.checked_next().expect("token 2"),
        digest,
    )
    .expect("valid advanced checkpoint fact")
}

#[test]
fn registration_publish_enumerate_and_lookup_by_name() {
    let root = TempDir::new("discovery");
    let directory = CellDirectory::open(root.path()).expect("open directory");

    let first = directory
        .publish_registration(&registration(0xa1, 1))
        .expect("publish first");
    let second = directory
        .publish_registration(&registration(0xa2, 1))
        .expect("publish second");

    // The publisher stamps the wall heartbeat; the counter passes through.
    assert_eq!(first.heartbeat(), 1);
    assert!(first.heartbeat_ms() > 0);
    assert!(second.heartbeat_ms() > 0);

    let snapshot = directory.snapshot().expect("snapshot");
    assert_eq!(snapshot.corrupt(), 0);
    let entries = snapshot.into_entries();
    assert_eq!(entries.len(), 2, "both published Cells must enumerate");
    assert_eq!(entries[0].identity(), identity(0xa1), "sorted by identity");
    assert_eq!(entries[1].identity(), identity(0xa2));
    assert_eq!(
        entries
            .iter()
            .map(nlos_cell::CellRegistrationEntry::epoch)
            .collect::<Vec<_>>(),
        vec![CellEpoch::INITIAL, CellEpoch::INITIAL]
    );

    // By-name lookup: hit with the full record, miss as None.
    let found = directory
        .find(identity(0xa2))
        .expect("lookup")
        .expect("published Cell must be found by name");
    assert_eq!(found.identity(), identity(0xa2));
    assert_eq!(found.os_process_id(), std::process::id());
    assert_eq!(found.description(), "nlos-cell federation unit fixture");
    assert!(
        directory.find(identity(0xff)).expect("lookup").is_none(),
        "unknown name is a None miss"
    );

    // Live window: freshly published, both inside a generous window.
    let live = directory.live_cells(60_000).expect("live cells");
    assert_eq!(live.len(), 2);
}

#[test]
fn registration_heartbeat_is_monotone_and_epoch_visible() {
    let root = TempDir::new("monotone");
    let directory = CellDirectory::open(root.path()).expect("open directory");

    directory
        .publish_registration(&registration(0xb1, 1))
        .expect("initial publish");

    // The Cell advanced an epoch; the refresh carries it at heartbeat 2.
    let advanced = nlos_cell::CellRegistrationEntry::new(
        identity(0xb1),
        Generation::INITIAL,
        CellEpoch::INITIAL.checked_next().expect("epoch 2"),
        CellFencingToken::INITIAL.checked_next().expect("token 2"),
        std::process::id(),
        "nlos-cell federation unit fixture",
    )
    .expect("valid registration")
    .with_heartbeat(2);
    let stamped = directory
        .publish_registration(&advanced)
        .expect("epoch refresh");
    assert!(stamped.heartbeat_ms() > 0);

    let found = directory
        .find(identity(0xb1))
        .expect("lookup")
        .expect("present");
    assert_eq!(found.epoch(), CellEpoch::INITIAL.checked_next().expect("2"));
    assert_eq!(found.heartbeat(), 2);

    // Same or lower heartbeat under the same boot: refused, on disk wins.
    assert_eq!(
        directory
            .publish_registration(&registration(0xb1, 2))
            .expect_err("equal heartbeat must refuse"),
        FederationError::RegistrationNotMonotone {
            on_disk_heartbeat: 2,
            presented_heartbeat: 2,
        }
    );
    assert_eq!(
        directory
            .publish_registration(&registration(0xb1, 1))
            .expect_err("lower heartbeat must refuse"),
        FederationError::RegistrationNotMonotone {
            on_disk_heartbeat: 2,
            presented_heartbeat: 1,
        }
    );

    // A restart (boot generation bumped) supersedes even at a lower
    // heartbeat counter: the newer boot is the newer registration.
    let restarted = nlos_cell::CellRegistrationEntry::new(
        identity(0xb1),
        Generation::INITIAL.checked_next().expect("boot 2"),
        CellEpoch::INITIAL,
        CellFencingToken::INITIAL,
        std::process::id(),
        "nlos-cell federation unit fixture",
    )
    .expect("valid registration")
    .with_heartbeat(1);
    directory
        .publish_registration(&restarted)
        .expect("new boot supersedes");
    let found = directory
        .find(identity(0xb1))
        .expect("lookup")
        .expect("present");
    assert_eq!(
        found.node_boot_generation(),
        Generation::INITIAL.checked_next().expect("boot 2")
    );
    assert_eq!(found.heartbeat(), 1);
}

#[test]
fn stale_and_corrupt_registrations_reported_not_hidden() {
    let root = TempDir::new("stale");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    directory
        .publish_registration(&registration(0xc1, 1))
        .expect("publish live cell");

    // A registration stamped long ago (format pinned by this face):
    // enumerated, but outside a short live window.
    let stale_hex = hex(&[0xc2; 16]);
    let stale_file = root.path().join("cells").join(format!("{stale_hex}.cell"));
    fs::write(
        &stale_file,
        format!(
            "cell={stale_hex}\nboot=1\nepoch=1\ntoken=1\npid={}\nheartbeat=1\nheartbeat_ms=0\nsummary=stale fixture cell\n",
            std::process::id()
        ),
    )
    .expect("write stale registration");
    // A corrupt file: counted, never silently hidden.
    fs::write(
        root.path()
            .join("cells")
            .join(format!("{}.cell", hex(&[0xc3; 16]))),
        "cell=not-hex\n",
    )
    .expect("write corrupt registration");

    let snapshot = directory.snapshot().expect("snapshot");
    assert_eq!(snapshot.corrupt(), 1, "the corrupt file is reported");
    let entries = snapshot.into_entries();
    assert_eq!(entries.len(), 2, "parseable stale cell still enumerates");

    let live = directory.live_cells(60_000).expect("live cells");
    assert_eq!(
        live.iter()
            .map(nlos_cell::CellRegistrationEntry::identity)
            .collect::<Vec<_>>(),
        vec![identity(0xc1)],
        "only the freshly stamped Cell is inside the silence window"
    );

    // Fail closed: publishing over a corrupt file of the same name refuses
    // instead of overwriting.
    assert_eq!(
        directory
            .publish_registration(&registration(0xc3, 1))
            .expect_err("corrupt target must refuse"),
        FederationError::StateCorrupt
    );
}

#[test]
fn migration_intents_append_in_order_and_round_trip() {
    let root = TempDir::new("intents");
    let directory = CellDirectory::open(root.path()).expect("open directory");

    let first = directory
        .record_migration_intent(&intent(0xd1, 0xd2, 0x01, 7))
        .expect("record first");
    let second = directory
        .record_migration_intent(&intent(0xd2, 0xd1, 0x02, 9))
        .expect("record second");
    // Re-stating the same intent is an append, not a dedupe: the log is a
    // log, intent state machines belong to the executor.
    let third = directory
        .record_migration_intent(&intent(0xd1, 0xd2, 0x01, 7))
        .expect("record restated");

    assert!(third.recorded_ms() >= second.recorded_ms());
    assert!(second.recorded_ms() >= first.recorded_ms());
    assert_eq!(third.sequence(), second.sequence() + 1);

    let log = directory.migration_intents().expect("enumerate intents");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(records.len(), 3, "append-only: restatement included");
    let order: Vec<u8> = records
        .iter()
        .map(|record| record.intent().object().as_bytes()[0])
        .collect();
    assert_eq!(order, vec![0x01, 0x02, 0x01], "append order preserved");

    let round_trip = &records[0].intent();
    assert_eq!(round_trip.source(), identity(0xd1));
    assert_eq!(round_trip.target(), identity(0xd2));
    assert_eq!(round_trip.object(), MigrationObject::from_bytes([0x01; 16]));
    assert_eq!(round_trip.generation().get(), 7);
    assert_eq!(round_trip.reason(), "w50-l2 unit fixture intent");
    assert_eq!(records[0].os_process_id(), std::process::id());
}

#[test]
fn migration_intent_validates_author_fields() {
    let root = TempDir::new("validation");
    let directory = CellDirectory::open(root.path()).expect("open directory");

    let build = |reason: &str| {
        MigrationIntent::new(
            identity(0xe1),
            identity(0xe2),
            MigrationObject::from_bytes([0xaa; 16]),
            Generation::INITIAL,
            reason,
        )
    };

    assert_eq!(
        build("valid reason").expect("valid").reason(),
        "valid reason"
    );
    assert_eq!(
        MigrationIntent::new(
            identity(0xe1),
            identity(0xe1),
            MigrationObject::from_bytes([0xaa; 16]),
            Generation::INITIAL,
            "same cell",
        )
        .expect_err("source must differ from target"),
        FederationError::InvalidIntent("source and target cells are the same"),
    );
    assert_eq!(
        build("").unwrap_err(),
        FederationError::InvalidIntent("reason"),
    );
    assert_eq!(
        build("two\nlines").unwrap_err(),
        FederationError::InvalidIntent("reason"),
    );
    let over_long = "x".repeat(257);
    assert_eq!(
        build(&over_long).unwrap_err(),
        FederationError::InvalidIntent("reason"),
    );

    // Nothing was recorded through the refusals above.
    assert_eq!(
        directory
            .migration_intents()
            .expect("enumerate")
            .into_records()
            .len(),
        0
    );
}

fn hex(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex nibble");
    }
    out
}

#[test]
fn checkpoints_append_in_order_and_round_trip() {
    let root = TempDir::new("checkpoints");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    let cell = identity(0xf1);

    let first = directory
        .record_checkpoint(cell, &fact("quota high-water 7/10"))
        .expect("record first");
    let second = directory
        .record_checkpoint(
            cell,
            &advanced_fact("quota high-water 3/10 after epoch advance"),
        )
        .expect("record second");
    // Re-stating the same fact is an append, not a dedupe: the log is a
    // log, checkpoint state machines belong to the (deferred) executor.
    let third = directory
        .record_checkpoint(cell, &fact("quota high-water 7/10"))
        .expect("record restated");

    assert!(third.recorded_ms() >= second.recorded_ms());
    assert!(second.recorded_ms() >= first.recorded_ms());
    // The sequence counter is process-unique, so sibling tests recording
    // concurrently interleave their bumps; the guarantee the filename
    // uniqueness relies on is strict monotonicity per recording thread.
    assert!(second.sequence() > first.sequence());
    assert!(third.sequence() > second.sequence());

    let log = directory.checkpoints(cell).expect("enumerate checkpoints");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(records.len(), 3, "append-only: restatement included");
    assert_eq!(records[0], first, "append order preserved");
    assert_eq!(records[1], second);
    assert_eq!(records[2], third);

    // Round trip: every axis the author and the log contributed survives.
    assert_eq!(records[1].cell(), cell);
    assert_eq!(
        records[1].fact().node_boot_generation(),
        Generation::INITIAL.checked_next().expect("boot 2")
    );
    assert_eq!(
        records[1].fact().epoch(),
        CellEpoch::INITIAL.checked_next().expect("epoch 2")
    );
    assert_eq!(
        records[1].fact().fencing_token(),
        CellFencingToken::INITIAL.checked_next().expect("token 2")
    );
    assert_eq!(
        records[1].fact().digest(),
        "quota high-water 3/10 after epoch advance"
    );
    assert_eq!(records[1].os_process_id(), std::process::id());
    assert!(records[1].recorded_ms() > 0);
}

#[test]
fn checkpoints_of_distinct_cells_stay_disjoint() {
    let root = TempDir::new("checkpoints-cells");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    let left = identity(0xf1);
    let right = identity(0xf2);

    let left_first = directory
        .record_checkpoint(left, &fact("left audit 1"))
        .expect("left 1");
    let right_first = directory
        .record_checkpoint(right, &fact("right audit 1"))
        .expect("right 1");
    let left_second = directory
        .record_checkpoint(left, &fact("left audit 2"))
        .expect("left 2");

    let left_log = directory.checkpoints(left).expect("left enumeration");
    assert_eq!(left_log.corrupt(), 0);
    assert_eq!(
        left_log.into_records(),
        vec![left_first, left_second],
        "left's audit trail holds exactly its own two records"
    );
    let right_log = directory.checkpoints(right).expect("right enumeration");
    assert_eq!(right_log.corrupt(), 0);
    assert_eq!(
        right_log.into_records(),
        vec![right_first],
        "right's audit trail holds exactly its own one record; no cross-talk"
    );

    // A Cell that registered no checkpoint enumerates empty, not an error.
    let absent = directory
        .checkpoints(identity(0xff))
        .expect("absent enumeration");
    assert_eq!(absent.corrupt(), 0);
    assert_eq!(absent.into_records().len(), 0);
}

#[test]
fn corrupt_checkpoint_files_reported_not_hidden() {
    let root = TempDir::new("checkpoints-corrupt");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    let cell = identity(0xf3);
    directory
        .record_checkpoint(cell, &fact("healthy audit"))
        .expect("record healthy");

    let prefix = hex(&[0xf3; 16]);
    let checkpoints = root.path().join("checkpoints");
    // A corrupt file under the Cell's prefix: counted, never silently
    // hidden (format pinned by this face).
    fs::write(
        checkpoints.join(format!(
            "{prefix}-00000000000000000001-0000000001-0000000000.ckpt"
        )),
        "cell=not-hex\n",
    )
    .expect("write corrupt checkpoint");
    // A parseable file misfiled under the Cell's prefix but naming another
    // Cell — a shape this face never writes: counted corrupt, never
    // silently adopted into the trail.
    let foreign_hex = hex(&[0xf4; 16]);
    fs::write(
        checkpoints.join(format!("{prefix}-00000000000000000002-0000000001-0000000001.ckpt")),
        format!(
            "cell={foreign_hex}\nboot=1\nepoch=1\ntoken=1\nrecorded_ms=2\npid={}\nsequence=1\ndigest=misfiled fixture\n",
            std::process::id()
        ),
    )
    .expect("write misfiled checkpoint");

    let log = directory.checkpoints(cell).expect("enumerate");
    assert_eq!(log.corrupt(), 2, "corrupt and misfiled files are reported");
    let records = log.into_records();
    assert_eq!(records.len(), 1, "the healthy record still enumerates");
    assert_eq!(records[0].fact().digest(), "healthy audit");

    // The debris under one Cell's prefix never leaks into another's read.
    let other = directory
        .checkpoints(identity(0xf4))
        .expect("foreign enumeration");
    assert_eq!(other.corrupt(), 0);
    assert_eq!(other.into_records().len(), 0);
}

#[test]
fn checkpoint_validates_digest_before_any_write() {
    let root = TempDir::new("checkpoints-validation");
    let directory = CellDirectory::open(root.path()).expect("open directory");
    let cell = identity(0xf5);
    let build = |digest: &str| {
        nlos_cell::CheckpointFact::new(
            Generation::INITIAL,
            CellEpoch::INITIAL,
            CellFencingToken::INITIAL,
            digest,
        )
    };

    assert_eq!(
        build("quota reconciled at high-water 9")
            .expect("valid")
            .digest(),
        "quota reconciled at high-water 9"
    );
    assert_eq!(
        build("").unwrap_err(),
        FederationError::InvalidCheckpoint("digest")
    );
    assert_eq!(
        build("two\nlines").unwrap_err(),
        FederationError::InvalidCheckpoint("digest")
    );
    let over_long = "x".repeat(257);
    assert_eq!(
        build(&over_long).unwrap_err(),
        FederationError::InvalidCheckpoint("digest")
    );

    // Nothing was recorded through the refusals above.
    let log = directory.checkpoints(cell).expect("enumerate");
    assert_eq!(log.corrupt(), 0);
    assert_eq!(log.into_records().len(), 0);
}
