//! One OS process = one Cell. Two Cells require two processes (ADR-0018).
//!
//! Parent-side checks live in one `#[test]` so they share a single process
//! claim. The child helper is an `#[ignore]` harness entry: it is never a
//! default vacuous pass, and is only invoked via an explicit child spawn
//! (`--ignored --exact child_claim_helper`) with env+tempfile signaling.
//!
//! Live overlap: the child claims, writes a receipt, then blocks on a release
//! tempfile until the parent has claimed and compared fences. No product IPC.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nlos_cell::{CellAuthority, CellError, CellIdentity};
use nlos_types::SchedulerDomainId;

const CHILD_OUT_ENV: &str = "NLOS_CELL_CHILD_OUT";
const CHILD_RELEASE_ENV: &str = "NLOS_CELL_CHILD_RELEASE";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);

fn parent_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xc1; 16])
}

fn child_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xc2; 16])
}

/// Harness-only entry for a second OS process. Marked `#[ignore]` so a plain
/// `cargo test` does not count a no-op return as a pass when env is unset.
#[test]
#[ignore = "spawned by two_os_process_authorities_not_two_in_process_threads"]
fn child_claim_helper() {
    let out_path = std::env::var(CHILD_OUT_ENV)
        .expect("NLOS_CELL_CHILD_OUT must be set; this ignored harness is not a default test");
    let release_path = std::env::var(CHILD_RELEASE_ENV)
        .expect("NLOS_CELL_CHILD_RELEASE must be set; this ignored harness is not a default test");

    let authority = CellAuthority::claim(child_domain()).expect("child claim");
    let fence = authority.fence();
    let line = format!(
        "{}\n{}\n{}\n{}\n{}\n",
        authority.os_process_id(),
        hex(authority.identity().as_bytes()),
        authority.node_boot_generation().get(),
        authority.epoch().get(),
        fence.fencing_token().get()
    );
    fs::write(&out_path, line).expect("write child receipt");

    // Stay alive until the parent claims and signals release — live dual Cell.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&release_path).exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for parent release at {release_path}"
        );
        thread::sleep(POLL);
    }
}

struct ChildReceipt {
    pid: u32,
    identity: String,
    boot: u64,
    epoch: u64,
    token: u64,
}

fn spawn_child_claim_helper(out_path: &Path, release_path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "child_claim_helper", "--nocapture", "--ignored"])
        .env(CHILD_OUT_ENV, out_path)
        .env(CHILD_RELEASE_ENV, release_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn child Cell process")
}

fn wait_for_child_receipt(child: &mut Child, out_path: &Path) {
    // Wait for child receipt while the child process is still running.
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
}

fn parse_child_receipt(receipt: &str) -> ChildReceipt {
    let mut lines = receipt.lines();
    let pid: u32 = lines
        .next()
        .expect("child pid")
        .parse()
        .expect("child pid u32");
    let identity = lines.next().expect("child identity").to_string();
    let boot: u64 = lines.next().expect("child boot").parse().expect("boot u64");
    let epoch: u64 = lines
        .next()
        .expect("child epoch")
        .parse()
        .expect("epoch u64");
    let token: u64 = lines
        .next()
        .expect("child fencing token")
        .parse()
        .expect("token u64");
    ChildReceipt {
        pid,
        identity,
        boot,
        epoch,
        token,
    }
}

fn claim_sole_in_process_authority() -> (CellAuthority, SchedulerDomainId) {
    let domain_a = parent_domain();
    let domain_b = SchedulerDomainId::from_bytes([0xc3; 16]);
    let (first, second) = std::thread::scope(|scope| {
        let left = scope.spawn(|| CellAuthority::claim(domain_a));
        let right = scope.spawn(|| CellAuthority::claim(domain_b));
        (left.join().expect("left"), right.join().expect("right"))
    });

    let wins = u8::from(first.is_ok()) + u8::from(second.is_ok());
    assert_eq!(
        wins, 1,
        "exactly one in-process claim may succeed; got {first:?} / {second:?}"
    );

    let parent = match (first, second) {
        (Ok(authority), Err(error)) | (Err(error), Ok(authority)) => {
            assert_eq!(authority.os_process_id(), std::process::id());
            match error {
                CellError::AlreadyClaimedInProcess { existing } => {
                    assert_eq!(existing, authority.identity());
                }
                other => panic!("expected AlreadyClaimedInProcess, got {other:?}"),
            }
            authority
        }
        other => panic!("expected one Ok and one AlreadyClaimed, got {other:?}"),
    };
    (parent, domain_b)
}

fn assert_live_fence_compare(
    parent: &CellAuthority,
    child: &ChildReceipt,
    domain_b: SchedulerDomainId,
) {
    assert_ne!(child.pid, std::process::id());
    assert_eq!(parent.os_process_id(), std::process::id());
    assert_ne!(child.pid, parent.os_process_id());
    assert_eq!(
        child.identity,
        hex(CellIdentity::from_domain(child_domain()).as_bytes())
    );
    assert!(
        parent.identity() == CellIdentity::from_domain(parent_domain())
            || parent.identity() == CellIdentity::from_domain(domain_b)
    );
    assert_ne!(hex(parent.identity().as_bytes()).as_str(), child.identity);

    // Live fence axes: distinct identities/pids; boot/epoch/token still INITIAL.
    assert_eq!(child.boot, 1);
    assert_eq!(child.epoch, 1);
    assert_eq!(child.token, 1);
    assert_eq!(parent.node_boot_generation().get(), 1);
    assert_eq!(parent.epoch().get(), 1);
    assert_eq!(parent.fencing_token().get(), 1);
    assert_ne!(
        parent.fence().identity(),
        CellIdentity::from_domain(child_domain())
    );
}

#[test]
fn two_os_process_authorities_not_two_in_process_threads() {
    let stamp = std::process::id();
    let out_path = std::env::temp_dir().join(format!("nlos-cell-w38e-child-{stamp}.txt"));
    let release_path = std::env::temp_dir().join(format!("nlos-cell-w38e-release-{stamp}.flag"));
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&release_path);

    let mut child = spawn_child_claim_helper(&out_path, &release_path);
    wait_for_child_receipt(&mut child, &out_path);

    // Live overlap: child still holds its Cell while parent claims.
    assert!(
        child.try_wait().expect("child try_wait").is_none(),
        "child must still be alive so this test observes two live Cells (env+tempfile harness)"
    );

    let receipt = parse_child_receipt(&fs::read_to_string(&out_path).expect("child receipt"));
    let (parent, domain_b) = claim_sole_in_process_authority();

    // Still live: both authorities exist concurrently (child blocked on release).
    assert!(
        child
            .try_wait()
            .expect("child try_wait after parent claim")
            .is_none(),
        "child exited before parent finished live fence compare; do not cite post-exit receipt as two live Cells"
    );

    let retry = CellAuthority::claim(SchedulerDomainId::from_bytes([0xc4; 16]));
    assert_eq!(
        retry.expect_err("third claim must fail"),
        CellError::AlreadyClaimedInProcess {
            existing: parent.identity(),
        }
    );

    assert_live_fence_compare(&parent, &receipt, domain_b);

    fs::write(&release_path, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "child Cell process failed: {status:?}");

    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&release_path);
}

fn hex(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(32);
    for byte in bytes {
        write!(out, "{byte:02x}").expect("write hex nibble");
    }
    out
}
