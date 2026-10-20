//! W50-L2: the single-host dual-Cell evidence of the federation mechanism
//! face (ADR-0019's declared precondition for cross-Cell semantics).
//!
//! Two independent OS processes each open one assembled [`CellHost`]
//! (ADR-0018: one process = one Cell) against one shared
//! [`CellDirectory`] root. The evidence under test, in order:
//!
//! 1. both Cells publish registrations the other can enumerate and look
//!    up by name (cross-Cell name/service discovery);
//! 2. the parent records exactly one [`MigrationIntent`] (registration
//!    only — no execution, `C-MIGRATE` owns that);
//! 3. both sides enumerate the same intent with the same axes.
//!
//! **Evidence scope (honesty bound):** this is *single-host, dual-process*
//! evidence — shared filesystem, shared kernel, shared clock domain. It is
//! exactly what ADR-0019 decision 1 defers cross-machine semantics behind;
//! citing it as cross-host / network-partition / cross-clock evidence
//! would violate RISK-B-11.
//!
//! Harness follows the `nlos-cell` dual-process convention: an `#[ignore]`
//! child helper spawned from this very test binary via env + temp-file
//! signaling, no product IPC. One `#[test]` so the parent claims the
//! process-scoped Cell exactly once.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nlos_cell::{
    CellDirectory, CellEpoch, CellFencingToken, CellIdentity, FailureDetectorConfig,
    MigrationIntent, MigrationObject,
};
use nlos_slice_k::{CellHost, CellHostConfig};
use nlos_types::{DeviceId, Generation, SchedulerDomainId};

const CELL_ROOT_ENV: &str = "NLOS_W50L2_CELL_ROOT";
const FEDERATION_ROOT_ENV: &str = "NLOS_W50L2_FEDERATION_ROOT";
const OUT_ENV: &str = "NLOS_W50L2_OUT";
const INTENT_MARKER_ENV: &str = "NLOS_W50L2_INTENT_MARKER";
const RELEASE_ENV: &str = "NLOS_W50L2_RELEASE";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);
/// Silence window for the live-discovery read: both processes publish
/// within milliseconds of each other in this harness.
const LIVE_WINDOW_MS: u64 = 60_000;
/// The object the recorded intent names (opaque to the mechanism face).
const INTENT_OBJECT: [u8; 16] = [0x50; 16];
/// The fenced generation the intent names.
const INTENT_GENERATION: u64 = 7;

fn parent_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xd1; 16])
}

fn child_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xd2; 16])
}

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("threshold ordering")
}

fn hex(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex nibble");
    }
    out
}

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("nlos-slice-k-w50l2-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp root");
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
            Err(error) => panic!("remove w50l2 temp root: {error}"),
        }
    }
}

/// Harness-only entry: the second Cell. Opens one federated [`CellHost`],
/// receipts "registered", waits for the parent's intent marker, then
/// proves the child-side half of the evidence — mutual discovery plus a
/// consistent single-intent enumeration — before blocking on release.
#[test]
#[ignore = "spawned by dual_cell_hosts_discover_each_other_and_enumerate_one_shared_migration_intent"]
fn child_federated_cell_helper() {
    // Batch `--ignored`/`--include-ignored` runs (nightly scale-probe job)
    // reach this entry without env; only the parent spawn makes it meaningful.
    let (Ok(cell_root), Ok(federation_root), Ok(out_path), Ok(intent_marker), Ok(release)) = (
        std::env::var(CELL_ROOT_ENV),
        std::env::var(FEDERATION_ROOT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(INTENT_MARKER_ENV),
        std::env::var(RELEASE_ENV),
    ) else {
        eprintln!("skipped: w50l2 child env unset (harness entry, parent spawn only)");
        return;
    };

    let host = CellHost::open_federated(
        Path::new(&cell_root),
        CellHostConfig::new(
            child_domain(),
            900,
            400,
            DeviceId::from_bytes([0xd2; 16]),
            detector_config(),
        ),
        Path::new(&federation_root),
    )
    .expect("child opens its federated CellHost");
    let identity = host.fence().identity();
    let directory = host
        .cell_directory()
        .expect("federated open carries the directory");

    // Its own registration is already visible (heartbeat 1 at open).
    let own = directory
        .find(identity)
        .expect("child lookup")
        .expect("child's own registration");
    assert_eq!(own.heartbeat(), 1);
    assert_eq!(own.epoch(), CellEpoch::INITIAL);
    assert_eq!(own.os_process_id(), std::process::id());

    fs::write(
        &out_path,
        format!(
            "registered\n{}\n{}\n",
            std::process::id(),
            hex(identity.as_bytes())
        ),
    )
    .expect("write child registered receipt");

    // Wait for the parent's intent record before reading the shared log.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&intent_marker).exists() {
        assert!(
            Instant::now() < deadline,
            "child timed out waiting for parent intent marker at {intent_marker}"
        );
        thread::sleep(POLL);
    }

    // Child-side discovery: two Cells, distinct pids, both with complete
    // opening fences — the second Cell of ADR-0018's minimal topology.
    let snapshot = directory.snapshot().expect("child snapshot");
    assert_eq!(snapshot.corrupt(), 0);
    let entries = snapshot.into_entries();
    assert_eq!(entries.len(), 2, "child must see both Cells");
    let parent_identity = CellIdentity::from_domain(parent_domain());
    let parent_entry = directory
        .find(parent_identity)
        .expect("parent lookup")
        .expect("child must discover the parent Cell by name");
    assert_ne!(
        parent_entry.os_process_id(),
        std::process::id(),
        "the other Cell must live in another OS process"
    );
    assert_eq!(parent_entry.epoch(), CellEpoch::INITIAL);
    assert_eq!(parent_entry.fencing_token(), CellFencingToken::INITIAL);
    assert_eq!(
        parent_entry.description(),
        "slice-k CellHost seven-piece assembly"
    );
    let live = directory
        .live_cells(LIVE_WINDOW_MS)
        .expect("child live read");
    assert_eq!(live.len(), 2, "both Cells freshly heartbeated");

    // Child-side intent enumeration: exactly the parent's one intent.
    let log = directory.migration_intents().expect("child intent read");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(
        records.len(),
        1,
        "child must see exactly the recorded intent"
    );
    let intent = records[0].intent();
    assert_eq!(intent.source(), parent_identity);
    assert_eq!(intent.target(), identity);
    assert_eq!(intent.object(), MigrationObject::from_bytes(INTENT_OBJECT));
    assert_eq!(intent.generation().get(), INTENT_GENERATION);
    assert_eq!(records[0].os_process_id(), parent_entry.os_process_id());

    let mut receipt = fs::read_to_string(&out_path).expect("read back receipt");
    receipt.push_str("intent_ok\n");
    fs::write(&out_path, receipt).expect("write child intent receipt");

    // Stay alive until the parent releases — live dual-Cell overlap.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&release).exists() {
        assert!(
            Instant::now() < deadline,
            "child timed out waiting for parent release at {release}"
        );
        thread::sleep(POLL);
    }
}

fn spawn_child(
    cell_root: &Path,
    federation_root: &Path,
    out_path: &Path,
    intent_marker: &Path,
    release: &Path,
) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_federated_cell_helper",
            "--nocapture",
            "--ignored",
        ])
        .env(CELL_ROOT_ENV, cell_root)
        .env(FEDERATION_ROOT_ENV, federation_root)
        .env(OUT_ENV, out_path)
        .env(INTENT_MARKER_ENV, intent_marker)
        .env(RELEASE_ENV, release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn second Cell process")
}

/// Polls `condition` while the child is still alive, panicking on early
/// child exit or timeout (the `nlos-cell` harness convention).
fn wait_while_alive(child: &mut Child, what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + CHILD_WAIT;
    while !condition() {
        match child.try_wait() {
            Ok(Some(status)) => {
                panic!("child exited before {what}: {status:?}");
            }
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Parses the child's stage-1 receipt: `registered`, its pid, its
/// identity hex — and checks the identity is the agreed second Cell.
fn parse_child_registration(receipt: &str) -> u32 {
    let mut lines = receipt.lines();
    assert_eq!(lines.next(), Some("registered"));
    let child_pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    assert_eq!(
        lines.next().expect("child identity line"),
        hex(CellIdentity::from_domain(child_domain()).as_bytes())
    );
    assert_ne!(child_pid, std::process::id());
    child_pid
}

/// The first Cell: opened federated by this (parent) test process.
fn open_parent_federated(parent_root: &Path, federation_root: &Path) -> CellHost {
    CellHost::open_federated(
        parent_root,
        CellHostConfig::new(
            parent_domain(),
            1000,
            500,
            DeviceId::from_bytes([0xd1; 16]),
            detector_config(),
        ),
        federation_root,
    )
    .expect("parent opens its federated CellHost")
}

/// Parent-side discovery half: both Cells enumerate, the child's record is
/// complete, names a different OS process, and both are inside the live
/// window.
fn assert_parent_side_discovery(directory: &CellDirectory, child_pid: u32) {
    let snapshot = directory.snapshot().expect("parent snapshot");
    assert_eq!(snapshot.corrupt(), 0);
    assert_eq!(
        snapshot.into_entries().len(),
        2,
        "parent must see both Cells"
    );
    let child_entry = directory
        .find(CellIdentity::from_domain(child_domain()))
        .expect("child lookup")
        .expect("parent must discover the child Cell by name");
    assert_eq!(child_entry.os_process_id(), child_pid);
    assert_eq!(child_entry.epoch(), CellEpoch::INITIAL);
    assert_eq!(
        child_entry.description(),
        "slice-k CellHost seven-piece assembly"
    );
    assert_eq!(directory.live_cells(LIVE_WINDOW_MS).expect("live").len(), 2);
}

#[test]
fn dual_cell_hosts_discover_each_other_and_enumerate_one_shared_migration_intent() {
    let stamp = std::process::id();
    let federation_root = TempDir::new("federation");
    let parent_root = TempDir::new("parent-cell");
    let child_root = TempDir::new("child-cell");
    let out_path = std::env::temp_dir().join(format!("nlos-slice-k-w50l2-out-{stamp}.txt"));
    let intent_marker = std::env::temp_dir().join(format!("nlos-slice-k-w50l2-go-{stamp}.flag"));
    let release = std::env::temp_dir().join(format!("nlos-slice-k-w50l2-release-{stamp}.flag"));
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&intent_marker);
    let _ = fs::remove_file(&release);

    let mut child = spawn_child(
        child_root.path(),
        federation_root.path(),
        &out_path,
        &intent_marker,
        &release,
    );

    // Stage 1: the second Cell process registered itself and is live.
    wait_while_alive(&mut child, "child registration receipt", || {
        out_path.exists()
    });
    let child_pid = parse_child_registration(&fs::read_to_string(&out_path).expect("receipt"));
    assert!(
        child.try_wait().expect("child alive").is_none(),
        "child Cell must still be live while the parent opens its own"
    );

    let mut host = open_parent_federated(parent_root.path(), federation_root.path());
    let parent_identity = host.fence().identity();
    let directory = host
        .cell_directory()
        .expect("federated open carries the directory");
    assert_parent_side_discovery(directory, child_pid);

    // One migration intent, registered (never executed): parent hands the
    // object to the child Cell at its fenced generation.
    let recorded = host
        .record_migration_intent(
            &MigrationIntent::new(
                parent_identity,
                CellIdentity::from_domain(child_domain()),
                MigrationObject::from_bytes(INTENT_OBJECT),
                Generation::new(NonZeroU64::new(INTENT_GENERATION).expect("nonzero generation")),
                "w50-l2 dual-cell evidence intent (registration only, no execution)",
            )
            .expect("valid intent"),
        )
        .expect("record through the host")
        .expect("federated host must record");
    assert_eq!(recorded.intent().source(), parent_identity);
    assert!(recorded.recorded_ms() > 0);

    fs::write(&intent_marker, b"go").expect("signal child to read the log");

    // Stage 2: the child proved its half and is still alive.
    wait_while_alive(&mut child, "child intent receipt", || {
        fs::read_to_string(&out_path).is_ok_and(|receipt| receipt.contains("intent_ok"))
    });
    assert!(
        child.try_wait().expect("child alive at evidence").is_none(),
        "both Cells must be live when the shared view is cited"
    );

    // Parent-side re-read: the same single intent, same axes the child saw.
    let log = host
        .cell_directory()
        .expect("directory re-read")
        .migration_intents()
        .expect("parent intent read");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], recorded);

    // A heartbeat refresh republishes at a monotone counter.
    let refreshed = host
        .refresh_cell_registration()
        .expect("refresh registration")
        .expect("federated host must refresh");
    assert_eq!(refreshed.heartbeat(), 2);
    assert_eq!(
        host.cell_directory()
            .expect("directory after refresh")
            .find(parent_identity)
            .expect("parent lookup")
            .expect("present")
            .heartbeat(),
        2
    );

    fs::write(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "child Cell process failed: {status:?}");

    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&intent_marker);
    let _ = fs::remove_file(&release);
}
