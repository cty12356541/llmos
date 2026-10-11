//! CS0-D injection-channel self-proof (C-SHARD plan R7): the
//! "both-Cells-alive + communication blocked" channel mechanism the H6
//! partition and dual-primary categories inject through, proven on its own
//! before any category sits on it.
//!
//! What this proves, concretely, with both OS processes alive throughout
//! (ADR-0018: one process = one Cell):
//!
//! 1. **Delivery works healthy**: one barrier demand round-trips through
//!    the temp-file channel and the old Cell serves one signed
//!    observation (`demand 1` in its served counter).
//! 2. **Parent-side cut blocks delivery, not life**: while the
//!    [`CuttableSignChannel`] cut is set, the successor's demand fails
//!    closed with the drive's own channel error and the old Cell's
//!    served-demand counter does not move — while the old Cell's
//!    heartbeat counter (a file outside the blocked channel) keeps
//!    advancing and `try_wait` keeps reporting the child alive. Channel
//!    cut ≠ process death.
//! 3. **Child-side block refuses leaked demands**: with the old Cell's
//!    block flag set first, a demand file written directly (bypassing
//!    the parent-side cut) is neither consumed nor answered — the
//!    endpoint half enforces the same refusal — and the old Cell stays
//!    alive and heartbeating through the whole window.
//! 4. **Restore resumes delivery**: after both flags clear, a demand
//!    round-trips again (`demand 2`).
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. The injected
//! "cut" is a delivery refusal at the harness channel, not an OS-level
//! network partition; citing it as cross-host / network-partition /
//! cross-clock evidence would violate RISK-B-11.

mod common;

use std::fs;
use std::num::NonZeroU64;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{
    CuttableSignChannel, FileSignChannel, bootstrap_barrier_signer, demand_count, fixed_hex,
    heartbeat_counter, hex, publish, spawn_fault_child, spawn_heartbeat, wait_for_file_while_alive,
    wait_heartbeat_at_least,
};
use ed25519_dalek::SigningKey;
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{BarrierObservationDemand, BarrierObservationSource as _, TakeoverDriveError};
use nlos_task::{ParticipantRecord, ParticipantType};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId, ReceiptId, TaskParticipantId};

const STORE_DIR_ENV: &str = "NLOS_CS0D_CHANNEL_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_CHANNEL_RELEASE";
const BLOCK_ENV: &str = "NLOS_CS0D_CHANNEL_BLOCK";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_CHANNEL_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_CHANNEL_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_CHANNEL_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_CHANNEL_RESP";

const SIGNER_SEED: u8 = 0xA7;

/// Harness-only entry: the old Cell of the channel self-proof. Bootstraps
/// the barrier signer in the shared identity root, publishes its
/// registration, then serves signed observations (block-flag aware) until
/// released, heartbeating throughout.
#[test]
#[ignore = "spawned by cut_channel_blocks_delivery_while_both_cells_stay_alive"]
fn child_channel_old_cell_helper() {
    let (
        Ok(store_dir),
        Ok(release),
        Ok(block),
        Ok(heartbeat),
        Ok(out),
        Ok(sign_request),
        Ok(sign_response),
    ) = (
        std::env::var(STORE_DIR_ENV),
        std::env::var(RELEASE_ENV),
        std::env::var(BLOCK_ENV),
        std::env::var(HEARTBEAT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
    )
    else {
        eprintln!("skipped: cs0d channel child env unset (harness entry, parent spawn only)");
        return;
    };
    let identity = IdentityAuthority::open(Path::new(&store_dir).join("identity"))
        .expect("old Cell opens the identity root");
    let binding = bootstrap_barrier_signer(&identity, SIGNER_SEED);
    publish(
        Path::new(&out),
        &format!(
            "registered\n{}\n{}\n{}\n{}\n",
            std::process::id(),
            hex(binding.principal_id.as_bytes()),
            hex(binding.control_domain_id.as_bytes()),
            hex(binding.key_id.as_bytes()),
        ),
    );
    let heartbeat_thread = spawn_heartbeat(Path::new(&heartbeat), Path::new(&release));
    common::serve_observations(
        Path::new(&sign_request),
        Path::new(&sign_response),
        Path::new(&release),
        Path::new(&block),
        Path::new(&heartbeat),
        &SigningKey::from_bytes(&[SIGNER_SEED; 32]),
    );
    heartbeat_thread.join().expect("heartbeat thread");
}

/// A synthetic but well-formed demand (the channel signs whatever it is
/// given; no store is involved in the channel self-proof).
fn synthetic_demand() -> BarrierObservationDemand {
    BarrierObservationDemand {
        takeover_receipt_id: ReceiptId::from_bytes([0x5C; 16]),
        participant: ParticipantRecord {
            participant_type: ParticipantType::TaskStore,
            participant_id: TaskParticipantId::from_bytes([0x5D; 16]),
            participant_generation: Generation::new(
                NonZeroU64::new(1).expect("nonzero generation"),
            ),
            admission_receipt_id: ReceiptId::from_bytes([0x5E; 16]),
        },
        fence_set_root: [0x5F; 32],
    }
}

/// Proves the injected channel mechanism itself: a cut blocks barrier
/// demand delivery while both Cells provably stay alive, and delivery
/// resumes after restore. This is the R7 gate every partition /
/// dual-primary category sits on.
#[test]
#[allow(clippy::too_many_lines)]
fn cut_channel_blocks_delivery_while_both_cells_stay_alive() {
    let store_dir = common::TempDir::new("channel-store");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-channel-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-channel-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-channel-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-channel-resp-{stamp}.txt"));
    let block = base.join(format!("nlos-slice-k-cs0d-channel-block-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-channel-release-{stamp}.flag"));
    for path in [
        &out,
        &heartbeat,
        &sign_request,
        &sign_response,
        &block,
        &release,
    ] {
        let _ = fs::remove_file(path);
    }

    let mut child = spawn_fault_child(
        "child_channel_old_cell_helper",
        &[
            (STORE_DIR_ENV, store_dir.path().as_os_str().to_os_string()),
            (RELEASE_ENV, release.as_os_str().to_os_string()),
            (BLOCK_ENV, block.as_os_str().to_os_string()),
            (HEARTBEAT_ENV, heartbeat.as_os_str().to_os_string()),
            (OUT_ENV, out.as_os_str().to_os_string()),
            (SIGN_REQUEST_ENV, sign_request.as_os_str().to_os_string()),
            (SIGN_RESPONSE_ENV, sign_response.as_os_str().to_os_string()),
        ],
    );

    // Stage 1: registration — the old Cell is up and serving.
    let registration = wait_for_file_while_alive(&mut child, "channel child registration", &out);
    let mut lines = registration.lines();
    assert_eq!(lines.next(), Some("registered"));
    let child_pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    let issuer = PrincipalId::from_bytes(fixed_hex(lines.next().expect("principal line")));
    let domain = ControlDomainId::from_bytes(fixed_hex(lines.next().expect("domain line")));
    let key_id = KeyId::from_bytes(fixed_hex(lines.next().expect("key line")));
    assert_ne!(child_pid, std::process::id());
    let hb_baseline = heartbeat_counter(&heartbeat);
    assert!(hb_baseline > 0, "the old Cell must be heartbeating already");

    // Stage 2: healthy delivery round-trips and is counted by the child.
    // The channel borrows the child only for the duration of one delivery
    // (construction is stateless), so liveness assertions between calls
    // keep their own access.
    let deliver = |cut: bool, child: &mut std::process::Child| {
        CuttableSignChannel::new(FileSignChannel {
            child,
            request_path: sign_request.clone(),
            response_path: sign_response.clone(),
            issuer,
            control_domain_id: domain,
            key_id,
        })
        .cut_if(cut)
        .observe_barrier(synthetic_demand())
    };
    let observation = deliver(false, &mut child).expect("healthy channel serves the demand");
    assert_eq!(observation.signature.key_id, key_id);
    let served_before_cut = demand_count(&heartbeat);
    assert_eq!(served_before_cut, 1, "exactly one demand served so far");

    // Stage 3: the parent-side cut. The demand fails closed with the
    // drive's own channel error; the child provably receives nothing
    // (served counter unmoved) while provably staying alive (heartbeat
    // advanced by several beats past the attempt, try_wait alive).
    let hb_at_cut = heartbeat_counter(&heartbeat);
    let cut_error = deliver(true, &mut child).expect_err("a cut channel must fail closed");
    assert!(
        matches!(&cut_error, TakeoverDriveError::BarrierSource(reason)
            if reason.contains("partition") && reason.contains("cut")),
        "expected the partition cut failure, got {cut_error}"
    );
    // Let >= 4 heartbeat beats pass — had a demand been delivered in the
    // cut window, the old Cell would have served it by now.
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_at_cut + 4);
    assert_eq!(
        demand_count(&heartbeat),
        served_before_cut,
        "no demand may be delivered while the channel is cut"
    );
    assert!(
        child.try_wait().expect("child alive during cut").is_none(),
        "the old Cell must stay alive through the cut window"
    );

    // Stage 4: the child-side block. The flag goes up FIRST; only after
    // the old Cell has looped at least once under the flag does a demand
    // file get written directly (bypassing the parent-side wrapper). The
    // old Cell neither consumes nor answers it — and still lives.
    fs::write(&block, b"block").expect("set the child-side block flag");
    let hb_at_block = heartbeat_counter(&heartbeat);
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_at_block + 2);
    fs::write(&sign_request, "leaked\npayload\n").expect("write a leaked demand file");
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_at_block + 6);
    assert!(
        !sign_response.exists(),
        "the blocked old Cell must not answer a leaked demand"
    );
    assert!(
        sign_request.exists(),
        "the blocked old Cell must not consume a leaked demand"
    );
    assert_eq!(demand_count(&heartbeat), served_before_cut);
    assert!(
        child
            .try_wait()
            .expect("child alive during block")
            .is_none(),
        "the old Cell must stay alive through the block window"
    );
    fs::remove_file(&sign_request).expect("clean the leaked demand");
    fs::remove_file(&block).expect("clear the block flag");

    // Stage 5: restore resumes delivery — the same demand round-trips and
    // the served counter advances to two.
    deliver(false, &mut child).expect("restored channel serves the demand again");
    let deadline = Instant::now() + Duration::from_secs(5);
    while demand_count(&heartbeat) < served_before_cut + 1 {
        assert!(
            Instant::now() < deadline,
            "the old Cell did not count the restored demand"
        );
        std::thread::sleep(common::POLL);
    }
    assert_eq!(
        demand_count(&heartbeat),
        served_before_cut + 1,
        "delivery resumed exactly once after restore"
    );
    assert!(
        child.try_wait().expect("child alive at restore").is_none(),
        "the old Cell must still be alive after restore"
    );

    fs::write(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "old Cell process failed: {status:?}");
    for path in [
        &out,
        &heartbeat,
        &sign_request,
        &sign_response,
        &block,
        &release,
    ] {
        let _ = fs::remove_file(path);
    }
}
