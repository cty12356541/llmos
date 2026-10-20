//! W51-L1: the single-host dual-Cell evidence of the reconciliation /
//! audit-checkpoint mechanism face — the third §26.1 control-plane
//! responsibility ADR-0019 decision 5 names as federation's mechanism
//! face ("reconciliation 和审计 checkpoint").
//!
//! Two independent OS processes each open one assembled federated
//! [`CellHost`] (ADR-0018: one process = one Cell) against one shared
//! [`CellDirectory`] root. The evidence under test, in order:
//!
//! 1. the child Cell registers exactly one checkpoint through the host
//!    wiring ([`CellHost::record_reconciliation_checkpoint`]) — its
//!    opening fence snapshot plus the agreed digest — and proves its own
//!    enumeration sees exactly that record;
//! 2. the parent Cell enumerates the child's audit trail through the same
//!    shared root and finds the identical record with identical axes
//!    (cell, boot, epoch, token, digest, recording pid, wall time);
//! 3. the parent's own registration lands in its own trail only: each
//!    Cell's enumeration stays disjoint — no cross-talk.
//!
//! **Evidence scope (honesty bound):** registration + enumeration only.
//! Nothing here verifies, executes, or coordinates a checkpoint — those
//! shapes stay deferred until ADR-0019's finalization. This is
//! *single-host, dual-process* evidence (shared filesystem, shared
//! kernel, shared clock domain); citing it as cross-host /
//! network-partition / cross-clock evidence would violate RISK-B-11.
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

use nlos_cell::{CellDirectory, CellIdentity, FailureDetectorConfig};
use nlos_slice_k::{CellHost, CellHostConfig};
use nlos_types::{DeviceId, SchedulerDomainId};

const CELL_ROOT_ENV: &str = "NLOS_W51L1_CELL_ROOT";
const FEDERATION_ROOT_ENV: &str = "NLOS_W51L1_FEDERATION_ROOT";
const OUT_ENV: &str = "NLOS_W51L1_OUT";
const RELEASE_ENV: &str = "NLOS_W51L1_RELEASE";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);
/// The agreed checkpoint digest both sides cite — the free summary axis
/// of the evidence (what the author claims reconciled).
const CHECKPOINT_DIGEST: &str =
    "w51-l1 dual-cell audit checkpoint: sub-ledgers reconciled at opening fence";

fn parent_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xe1; 16])
}

fn child_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xe2; 16])
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
            std::env::temp_dir().join(format!("nlos-slice-k-w51l1-{name}-{}", std::process::id()));
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
            Err(error) => panic!("remove w51l1 temp root: {error}"),
        }
    }
}

/// Harness-only entry: the second Cell. Opens one federated [`CellHost`],
/// registers its opening-fence checkpoint through the host wiring, proves
/// its own enumeration of that trail, receipts the fence axes the parent
/// will compare against, and blocks on release.
#[test]
#[ignore = "spawned by dual_cell_checkpoint_registered_by_child_enumerated_by_parent"]
fn child_checkpoint_cell_helper() {
    // Batch `--ignored`/`--include-ignored` runs (nightly scale-probe job)
    // reach this entry without env; only the parent spawn makes it meaningful.
    let (Ok(cell_root), Ok(federation_root), Ok(out_path), Ok(release)) = (
        std::env::var(CELL_ROOT_ENV),
        std::env::var(FEDERATION_ROOT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(RELEASE_ENV),
    ) else {
        eprintln!("skipped: w51l1 child env unset (harness entry, parent spawn only)");
        return;
    };

    let host = CellHost::open_federated(
        Path::new(&cell_root),
        CellHostConfig::new(
            child_domain(),
            700,
            300,
            DeviceId::from_bytes([0xe2; 16]),
            detector_config(),
        ),
        Path::new(&federation_root),
    )
    .expect("child opens its federated CellHost");
    let fence = host.fence();
    let identity = fence.identity();

    // The registration lane under evidence: host wiring turns the current
    // fence snapshot plus the agreed digest into one checkpoint record.
    let recorded = host
        .record_reconciliation_checkpoint(CHECKPOINT_DIGEST)
        .expect("child records its checkpoint")
        .expect("federated host must record");
    assert_eq!(recorded.cell(), identity);
    assert_eq!(
        recorded.fact().node_boot_generation(),
        fence.node_boot_generation()
    );
    assert_eq!(recorded.fact().epoch(), fence.epoch());
    assert_eq!(recorded.fact().fencing_token(), fence.fencing_token());
    assert_eq!(recorded.fact().digest(), CHECKPOINT_DIGEST);
    assert_eq!(recorded.os_process_id(), std::process::id());

    // Child-side enumeration: exactly its own one record, same axes.
    let directory = host
        .cell_directory()
        .expect("federated open carries the directory");
    let log = directory.checkpoints(identity).expect("child trail read");
    assert_eq!(log.corrupt(), 0);
    let records = log.into_records();
    assert_eq!(
        records.len(),
        1,
        "child must see exactly its own checkpoint"
    );
    assert_eq!(records[0], recorded);
    assert!(records[0].recorded_ms() > 0);

    // Receipt: the fence axes the parent's comparison needs (the boot
    // generation is assigned at claim time, so it travels by receipt).
    fs::write(
        &out_path,
        format!(
            "checkpointed\n{}\n{}\n{}\n{}\n{}\naudit_ok\n",
            std::process::id(),
            hex(identity.as_bytes()),
            fence.node_boot_generation().get(),
            fence.epoch().get(),
            fence.fencing_token().get()
        ),
    )
    .expect("write child checkpoint receipt");

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

fn spawn_child(cell_root: &Path, federation_root: &Path, out_path: &Path, release: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_checkpoint_cell_helper",
            "--nocapture",
            "--ignored",
        ])
        .env(CELL_ROOT_ENV, cell_root)
        .env(FEDERATION_ROOT_ENV, federation_root)
        .env(OUT_ENV, out_path)
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

/// The child's receipt: its pid, identity hex, and the fence axes (boot,
/// epoch, token) its checkpoint was taken at — parsed and type-checked so
/// the parent's comparison below is against values, not trust.
struct ChildReceipt {
    pid: u32,
    identity: CellIdentity,
    boot: u64,
    epoch: u64,
    token: u64,
}

fn parse_child_receipt(receipt: &str) -> ChildReceipt {
    let mut lines = receipt.lines();
    assert_eq!(lines.next(), Some("checkpointed"));
    let pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    let identity_hex = lines.next().expect("child identity line");
    assert_eq!(
        identity_hex,
        hex(CellIdentity::from_domain(child_domain()).as_bytes()),
        "the checkpointing Cell must be the agreed second Cell"
    );
    let boot: u64 = lines
        .next()
        .expect("child boot line")
        .parse()
        .expect("child boot u64");
    let epoch: u64 = lines
        .next()
        .expect("child epoch line")
        .parse()
        .expect("child epoch u64");
    let token: u64 = lines
        .next()
        .expect("child token line")
        .parse()
        .expect("child token u64");
    assert_eq!(lines.next(), Some("audit_ok"));
    assert_ne!(pid, std::process::id());
    ChildReceipt {
        pid,
        identity: CellIdentity::from_domain(child_domain()),
        boot,
        epoch,
        token,
    }
}

#[test]
fn dual_cell_checkpoint_registered_by_child_enumerated_by_parent() {
    let stamp = std::process::id();
    let federation_root = TempDir::new("federation");
    let parent_root = TempDir::new("parent-cell");
    let child_root = TempDir::new("child-cell");
    let out_path = std::env::temp_dir().join(format!("nlos-slice-k-w51l1-out-{stamp}.txt"));
    let release = std::env::temp_dir().join(format!("nlos-slice-k-w51l1-release-{stamp}.flag"));
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&release);

    let mut child = spawn_child(
        child_root.path(),
        federation_root.path(),
        &out_path,
        &release,
    );

    // Stage 1: the second Cell process registered its checkpoint and is
    // still live.
    wait_while_alive(&mut child, "child checkpoint receipt", || {
        fs::read_to_string(&out_path).is_ok_and(|receipt| receipt.contains("audit_ok"))
    });
    let receipt = parse_child_receipt(&fs::read_to_string(&out_path).expect("checkpoint receipt"));
    assert!(
        child.try_wait().expect("child alive").is_none(),
        "child Cell must still be live while the parent opens its own"
    );

    // Stage 2: the parent's enumeration of the child's audit trail, over
    // the same shared root, through its own federated handle.
    let host = CellHost::open_federated(
        parent_root.path(),
        CellHostConfig::new(
            parent_domain(),
            800,
            200,
            DeviceId::from_bytes([0xe1; 16]),
            detector_config(),
        ),
        federation_root.path(),
    )
    .expect("parent opens its federated CellHost");
    let directory: &CellDirectory = host
        .cell_directory()
        .expect("federated open carries the directory");

    let child_log = directory
        .checkpoints(receipt.identity)
        .expect("parent enumerates the child trail");
    assert_eq!(child_log.corrupt(), 0);
    let child_records = child_log.into_records();
    assert_eq!(
        child_records.len(),
        1,
        "parent must see exactly the child's one checkpoint"
    );
    let record = &child_records[0];
    assert_eq!(record.cell(), receipt.identity);
    assert_eq!(record.fact().node_boot_generation().get(), receipt.boot);
    assert_eq!(record.fact().epoch().get(), receipt.epoch);
    assert_eq!(record.fact().fencing_token().get(), receipt.token);
    assert_eq!(record.fact().digest(), CHECKPOINT_DIGEST);
    assert_eq!(record.os_process_id(), receipt.pid);
    assert!(record.recorded_ms() > 0);

    // Stage 3: the parent's own registration lands in its own trail only.
    let parent_identity = host.fence().identity();
    let parent_recorded = host
        .record_reconciliation_checkpoint(CHECKPOINT_DIGEST)
        .expect("parent records its checkpoint")
        .expect("federated host must record");
    assert_eq!(parent_recorded.cell(), parent_identity);
    assert_eq!(parent_recorded.os_process_id(), std::process::id());

    let parent_log = directory
        .checkpoints(parent_identity)
        .expect("parent enumerates its own trail");
    assert_eq!(parent_log.corrupt(), 0);
    assert_eq!(
        parent_log.into_records(),
        vec![parent_recorded],
        "parent's trail holds exactly its own one record"
    );
    let child_log_after = directory
        .checkpoints(receipt.identity)
        .expect("parent re-enumerates the child trail");
    assert_eq!(child_log_after.corrupt(), 0);
    assert_eq!(
        child_log_after.into_records(),
        child_records,
        "the parent's append must not leak into the child's trail"
    );
    assert!(
        child.try_wait().expect("child alive at evidence").is_none(),
        "both Cells must be live when the shared view is cited"
    );

    fs::write(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "child Cell process failed: {status:?}");

    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&release);
}
