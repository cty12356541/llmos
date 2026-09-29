//! Persist `node_boot_generation` in a caller-supplied local data directory.
//!
//! ADR-0018 / Stage C ruling: each Cell process owns its own durable root.
//! A new process claim of the same identity after restart must present a
//! newer boot generation; `admit` rejects the previous boot (`MODEL-ID-003`).
//!
//! Restart is simulated with a second OS process (env + tempfile harness only;
//! no product IPC). One default `#[test]` claims the parent process once.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nlos_cell::{
    CellAdmitError, CellAuthority, CellEpoch, CellFence, CellFencingToken, CellIdentity,
};
use nlos_types::{Generation, SchedulerDomainId};

const CHILD_OUT_ENV: &str = "NLOS_CELL_BOOT_CHILD_OUT";
const CHILD_DATA_DIR_ENV: &str = "NLOS_CELL_BOOT_DATA_DIR";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);

fn domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xb0; 16])
}

/// Harness-only entry: claim with the shared data dir, write a fence receipt,
/// then exit so the parent can reclaim the same identity after "restart".
#[test]
#[ignore = "spawned by restart_bumps_boot_generation_and_admit_rejects_prior_boot"]
fn child_boot_claim_helper() {
    let out_path = std::env::var(CHILD_OUT_ENV)
        .expect("NLOS_CELL_BOOT_CHILD_OUT must be set; this ignored harness is not a default test");
    let data_dir = std::env::var(CHILD_DATA_DIR_ENV)
        .expect("NLOS_CELL_BOOT_DATA_DIR must be set; this ignored harness is not a default test");

    let authority =
        CellAuthority::claim_with_data_dir(domain(), Path::new(&data_dir)).expect("child claim");
    let fence = authority.fence();
    let line = format!(
        "{}\n{}\n{}\n{}\n",
        authority.node_boot_generation().get(),
        fence.epoch().get(),
        fence.fencing_token().get(),
        hex(authority.identity().as_bytes()),
    );
    fs::write(&out_path, line).expect("write child receipt");
}

#[test]
fn restart_bumps_boot_generation_and_admit_rejects_prior_boot() {
    let stamp = std::process::id();
    let data_dir: PathBuf = std::env::temp_dir().join(format!("nlos-cell-w38-boot-data-{stamp}"));
    let out_path = std::env::temp_dir().join(format!("nlos-cell-w38-boot-child-{stamp}.txt"));
    let _ = fs::remove_dir_all(&data_dir);
    let _ = fs::remove_file(&out_path);
    fs::create_dir_all(&data_dir).expect("create data dir");

    let mut child = Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_boot_claim_helper",
            "--nocapture",
            "--ignored",
        ])
        .env(CHILD_OUT_ENV, &out_path)
        .env(CHILD_DATA_DIR_ENV, &data_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child Cell process");

    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if out_path.exists() {
            break;
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                panic!("child exited before writing receipt: {status:?}");
            }
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for child receipt at {}",
            out_path.display()
        );
        thread::sleep(POLL);
    }

    let status = child.wait().expect("wait child exit (restart boundary)");
    assert!(status.success(), "child Cell process failed: {status:?}");

    let receipt = fs::read_to_string(&out_path).expect("child receipt");
    let mut lines = receipt.lines();
    let prior_boot: u64 = lines.next().expect("prior boot").parse().expect("boot u64");
    let prior_epoch: u64 = lines
        .next()
        .expect("prior epoch")
        .parse()
        .expect("epoch u64");
    let prior_token: u64 = lines
        .next()
        .expect("prior token")
        .parse()
        .expect("token u64");
    let prior_identity = lines.next().expect("prior identity");
    assert_eq!(prior_boot, Generation::INITIAL.get());
    assert_eq!(prior_epoch, CellEpoch::INITIAL.get());
    assert_eq!(prior_token, CellFencingToken::INITIAL.get());
    assert_eq!(
        prior_identity,
        hex(CellIdentity::from_domain(domain()).as_bytes())
    );

    // New process claim of the same identity + data dir after restart.
    let authority =
        CellAuthority::claim_with_data_dir(domain(), &data_dir).expect("parent reclaim");
    let current_boot = authority.node_boot_generation();
    assert_eq!(
        current_boot,
        Generation::INITIAL.checked_next().expect("boot 2"),
        "restart must present a newer node_boot_generation"
    );
    assert_eq!(authority.identity(), CellIdentity::from_domain(domain()));
    assert_eq!(authority.epoch(), CellEpoch::INITIAL);
    assert_eq!(authority.fencing_token(), CellFencingToken::INITIAL);

    let prior_fence = CellFence::present(
        CellIdentity::from_domain(domain()),
        Generation::new(NonZeroU64::new(prior_boot).expect("prior boot generation")),
        CellEpoch::from_u64(prior_epoch).expect("prior epoch"),
        CellFencingToken::from_u64(prior_token).expect("prior token"),
    );
    assert_eq!(
        authority.admit(&prior_fence),
        Err(CellAdmitError::BootGenerationMismatch {
            presented: Generation::INITIAL,
            current: current_boot,
        })
    );
    authority
        .admit(&authority.fence())
        .expect("current boot fence must admit");

    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_dir_all(&data_dir);
}

fn hex(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex nibble");
    }
    out
}
