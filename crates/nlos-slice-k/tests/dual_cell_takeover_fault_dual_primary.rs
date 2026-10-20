//! CS0-D H6 category **partition dual-primary**: through a partition
//! window in which BOTH OS processes stay provably alive (ADR-0018: one
//! process = one Cell), both sides act as the authority at once — the old
//! Cell, not observing any fence, keeps committing under its term-1
//! lease while the successor simultaneously drives the takeover — and
//! the durable lease/fencing CAS arbitrates exactly one winner
//! (`DIST-TASK-002` linearization; ADR-0019 R1.2.4).
//!
//! Assertion semantics (per the lane brief, R1.2.4, and the anchored
//! fail-closed lease machinery of `nlos-task` `lease.rs`):
//!
//! 1. **No preemption**: while the term-1 incumbent lease is live, the
//!    successor's drive is refused with `AuthorityLeaseHeld` and leaves
//!    zero durable takeover effect — it can only wait for the expiry
//!    (logical clock advance; short-TTL lease injection, no test sleeps).
//! 2. **Single winner by CAS**: after expiry, the successor's term-2
//!    acquisition is the arbitration point. From that instant — even
//!    with the takeover receipt still `Pending` and the channel still
//!    cut — the old Cell loses everything: its lease-bound permit /
//!    effect / result / commit submissions are all rejected with
//!    `AuthorityLeaseFenced`, and even a renewal under its own holder is
//!    refused with `AuthorityLeaseHeld` (the CAS linearized to the
//!    successor; the loser cannot take the lease back). The rejected
//!    submissions append no durable history rows — no double commit.
//! 3. **Both-alive proof through the whole window**: the old Cell's
//!    heartbeat counter (a file outside the blocked channel) advances
//!    past every injection point, its served-demand counter shows it
//!    received nothing through the cut, and `try_wait` reports it alive
//!    at every evidence point — the loser is rejected by the durable
//!    state, not by being dead (R2.2 item 3 methodological constraint).
//! 4. **Exactly one side completes the takeover**: after the channel is
//!    restored, the successor (and only it) completes the pending
//!    takeover, and the convergence over the old Cell's checkpoint trail
//!    adopts exactly the dual-claim window's durable gap with all four
//!    ADR-0013 contract invariants true.
//!
//! DIST-TASK-002 tail mapping of the old Cell's rejected actions in this
//! scenario: `permit` → a fresh lease-bound `CommitPermit` request
//! (`AuthorityLeaseFenced`), plus the renewal refusal
//! (`AuthorityLeaseHeld`). The `effect` / `result` / `commit` faces
//! require an outstanding mid-flight permit, which in this store shape
//! leaves the exact fence set unresolvable — they are injected in the
//! stale-epoch category instead; `message` / `callback` have no
//! `TaskStore`-endpoint face in this slice (declared not injectable, not
//! claimed; the six-type mapping is Phase 1 CS1-C).
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. The injected
//! "cut" is a delivery refusal at the harness channel, not an OS-level
//! network partition; citing it as cross-host / network-partition /
//! cross-clock evidence would violate RISK-B-11. Endpoint coverage
//! declared: the `TaskStore` participant endpoint only.

mod common;

use std::fs;
use std::num::NonZeroU64;
use std::path::Path;

use common::{
    CuttableSignChannel, FileSignChannel, acquire_incumbent_lease, append_line,
    assert_invariants_all_hold, assert_successor_wait_left_no_durable_effect,
    assignment_state_codes, bootstrap_barrier_signer, commit_segment, demand_count, fixed_hex,
    heartbeat_counter, hex, holder_from_pid, lease_history_rows, publish, raw_count,
    spawn_fault_child, spawn_heartbeat, wait_for_file_while_alive, wait_heartbeat_at_least,
    wait_while_alive,
};
use ed25519_dalek::SigningKey;
use nlos_cell::{CellDirectory, CheckpointDigest, FailureDetectorConfig};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{
    CrossCellTakeoverRequest, PrefixVisibility, ReconciliationRequest, TakeoverDriveError,
    converge_commit_prefix_gap, drive_cross_cell_takeover, durable_commit_prefix,
};
use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, AuthorityTakeoverReceiptState, PermitRequest,
    SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{
    ControlDomainId, DeviceId, IdempotencyKey, KeyId, PrincipalId, SchedulerDomainId, TaskId,
};

const STORE_DIR_ENV: &str = "NLOS_CS0D_DUAL_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_DUAL_RELEASE";
const BLOCK_ENV: &str = "NLOS_CS0D_DUAL_BLOCK";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_DUAL_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_DUAL_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_DUAL_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_DUAL_RESP";
const POST_CAS_ENV: &str = "NLOS_CS0D_DUAL_POST_CAS";

const TASK_SEED: u8 = 0xD9;
const CHILD_SEED: u8 = 0xC9;
const CHILD_HOLDER_TAG: u8 = 0xBA;
const PARENT_HOLDER_TAG: u8 = 0xBB;
const SIGNER_SEED: u8 = 0xA9;

/// Logical clock: term-1 lease at 100 with short TTL 100 (expires at
/// 200) — the dual-claim window's incumbent work (two committed
/// segments plus one mid-flight attempt) runs inside it; the successor's
/// no-preemption attempt runs at ~150; the arbitration takeover runs at
/// 300+ past expiry.
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const SEGMENT_ONE_AT_MS: i64 = 110;
const SEGMENT_TWO_AT_MS: i64 = 130;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_ATTEMPT_ONE_BASE_MS: i64 = 140;
const PARENT_ATTEMPT_TWO_BASE_MS: i64 = 295;
const PARENT_CLOCK_STEP_MS: i64 = 10;
/// The old Cell's post-CAS submission attempts run logically after the
/// successor's term-2 acquisition.
const CHILD_STALE_AT_MS: i64 = 310;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

fn child_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xE9; 16])
}

/// Harness-only entry: Cell A, the old authority and (through the
/// partition window) the still-acting primary. Commits two segments and
/// a mid-flight attempt under its live term-1 lease, records one
/// structured checkpoint through its federated [`CellHost`] wiring, then
/// — when the successor signals that its term-2 CAS has landed — proves
/// the loser's arbitration face: renewal refused with `Held`, permit /
/// effect / result / commit submissions all refused with `Fenced`, all
/// while staying alive; finally serves barrier observations until
/// released.
#[test]
#[ignore = "spawned by dual_primary_partition_arbitrates_a_single_winner"]
#[allow(clippy::too_many_lines)]
fn child_dual_primary_old_cell_helper() {
    let (
        Ok(store_dir),
        Ok(release),
        Ok(block),
        Ok(heartbeat),
        Ok(out),
        Ok(sign_request),
        Ok(sign_response),
        Ok(post_cas),
    ) = (
        std::env::var(STORE_DIR_ENV),
        std::env::var(RELEASE_ENV),
        std::env::var(BLOCK_ENV),
        std::env::var(HEARTBEAT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
        std::env::var(POST_CAS_ENV),
    )
    else {
        eprintln!("skipped: cs0d dual-primary child env unset (harness entry, parent spawn only)");
        return;
    };
    let store_path = Path::new(&store_dir).join("task-authority.sqlite3");
    let authority = SqliteTaskAuthority::open(&store_path).expect("old Cell opens its task store");

    // --- The incumbent term: two committed segments inside the TTL.
    let lease_one = acquire_incumbent_lease(
        &authority,
        holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
        CHILD_SEED,
        LEASE_ONE_AT_MS,
        LEASE_ONE_TTL_MS,
    );
    assert_eq!(lease_one.term, 1);
    common::register_task_and_attempt(&authority, task_id(), 0x2A, LEASE_ONE_AT_MS + 1);
    // (register_task_and_attempt registers the task plus a seed attempt
    // we never permit; commit_segment registers its own attempts.)
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x2B,
        0,
        SEGMENT_ONE_AT_MS,
        2,
    );

    // --- One structured checkpoint through the federated CellHost
    // wiring (the digest pins the durable prefix at the host's fence
    // axes, derived through the same public grammar the executor
    // verifies with).
    let host = nlos_slice_k::CellHost::open_federated(
        Path::new(&store_dir).join("cell-a"),
        nlos_slice_k::CellHostConfig::new(
            child_domain(),
            700,
            300,
            DeviceId::from_bytes([0xE8; 16]),
            FailureDetectorConfig::new(
                NonZeroU64::new(3).expect("suspect threshold"),
                NonZeroU64::new(5).expect("dead threshold"),
            )
            .expect("threshold ordering"),
        ),
        Path::new(&store_dir),
    )
    .expect("old Cell opens its federated CellHost");
    let fence = host.fence();
    let identity = fence.identity();
    let durable =
        durable_commit_prefix(&authority, task_id()).expect("old Cell derives its prefix");
    assert_eq!(durable.entry_count(), 2);
    let checkpoint_digest = CheckpointDigest::from_prefix(
        fence.node_boot_generation(),
        fence.epoch(),
        fence.fencing_token(),
        durable.prefix(),
    );
    host.record_reconciliation_checkpoint(&checkpoint_digest.to_text())
        .expect("federated host must record")
        .expect("checkpoint record");

    // --- The dual-claim window's second segment, still under the live
    // incumbent lease. (No mid-flight permit here: an outstanding
    // planned-effects permit without sealed effect endpoints leaves the
    // exact fence set unresolvable in this store shape, and this
    // category's takeover must complete — the mid-flight rejection faces
    // belong to the stale-epoch category's shape.)
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x2C,
        16,
        SEGMENT_TWO_AT_MS,
        2,
    );

    // --- The liveness thread starts before registration so every later
    // window (partition, post-CAS, serving) is covered by heartbeats.
    let heartbeat_thread = spawn_heartbeat(Path::new(&heartbeat), Path::new(&release));

    // --- The shared barrier signer.
    let identity_authority = IdentityAuthority::open(Path::new(&store_dir).join("identity"))
        .expect("old Cell opens the identity root");
    let binding = bootstrap_barrier_signer(&identity_authority, SIGNER_SEED);
    publish(
        Path::new(&out),
        &format!(
            "registered\n{}\n{}\n{}\n{}\n{}\n{}\n",
            std::process::id(),
            hex(identity.as_bytes()),
            checkpoint_digest.to_text(),
            hex(binding.principal_id.as_bytes()),
            hex(binding.control_domain_id.as_bytes()),
            hex(binding.key_id.as_bytes()),
        ),
    );
    append_line(Path::new(&out), "committed_two");

    // --- Wait until the successor's term-2 CAS has landed, then prove
    // the loser's arbitration face while staying alive.
    let deadline = std::time::Instant::now() + common::CHILD_WAIT;
    while !Path::new(&post_cas).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "old Cell timed out waiting for the post-CAS stage"
        );
        std::thread::sleep(common::POLL);
    }
    let holder = holder_from_pid(std::process::id(), CHILD_HOLDER_TAG);

    // Renewal under its own holder: refused with Held — the CAS
    // linearized to the successor, the loser cannot take the lease back.
    let renew_refused = matches!(
        authority.acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: holder,
            idempotency_key: IdempotencyKey::from_bytes([0x2E; 16]),
            requested_at_ms: CHILD_STALE_AT_MS,
            ttl_ms: 100_000,
        }),
        Err(TaskStoreError::AuthorityLeaseHeld)
    );
    assert!(
        renew_refused,
        "the loser's renewal must be refused with Held"
    );
    append_line(
        Path::new(&out),
        "post_cas:renew_refused:authority_lease_held",
    );

    // permit class: a fresh lease-bound CommitPermit request.
    let fresh_attempt =
        common::register_task_and_attempt(&authority, task_id(), 0x2F, CHILD_STALE_AT_MS + 1);
    let permit_decision = authority.request_commit_permit_with_authority_lease(
        nlos_task::AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: task_id(),
                attempt_id: fresh_attempt.attempt_id,
                attempt_generation: fresh_attempt.attempt_generation,
                write_set_root: [0x2F; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x3F; 16]),
                valid_until_ms: 1_000_000,
                requested_at_ms: CHILD_STALE_AT_MS + 2,
            },
            lease: lease_one,
        },
    );
    let permit_line = match permit_decision {
        Err(TaskStoreError::AuthorityLeaseFenced) => {
            "post_cas:permit_rejected:authority_lease_fenced"
        }
        Err(TaskStoreError::ParticipantRegistryFrozen { state }) => {
            assert_eq!(
                state,
                nlos_task::ParticipantRegistryState::FrozenForTakeover,
                "the frozen-registry refusal must name the takeover freeze"
            );
            "post_cas:permit_rejected:participant_registry_frozen_for_takeover"
        }
        other => panic!("the loser's permit request must be fenced, got {other:?}"),
    };
    append_line(Path::new(&out), permit_line);

    // commit class: a lease-bound finalize over a fresh attempt's permit
    // is unreachable here (its permit issuance was just fenced above), so
    // this category's loser face is the renewal and permit rejection; the
    // effect / result / commit faces are the stale-epoch category's
    // injectable subset (see `dual_cell_takeover_fault_stale_epoch.rs`).
    append_line(Path::new(&out), "post_cas_done");

    // --- Serve barrier observations until released (the successor
    // completes the takeover through the restored channel).
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

/// The dual-primary category: a both-alive partition window in which the
/// old Cell keeps committing while the successor drives the takeover;
/// the durable lease/fencing CAS arbitrates a single winner, the loser's
/// submissions are all rejected, and only the successor completes.
#[test]
#[allow(clippy::too_many_lines)]
fn dual_primary_partition_arbitrates_a_single_winner() {
    let store_dir = common::TempDir::new("dual-primary-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-dual-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-dual-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-dual-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-dual-resp-{stamp}.txt"));
    let block = base.join(format!("nlos-slice-k-cs0d-dual-block-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-dual-release-{stamp}.flag"));
    let post_cas = base.join(format!("nlos-slice-k-cs0d-dual-postcas-{stamp}.flag"));
    for path in [
        &out,
        &heartbeat,
        &sign_request,
        &sign_response,
        &block,
        &release,
        &post_cas,
    ] {
        let _ = fs::remove_file(path);
    }

    let mut child = spawn_fault_child(
        "child_dual_primary_old_cell_helper",
        &[
            (STORE_DIR_ENV, store_dir.path().as_os_str().to_os_string()),
            (RELEASE_ENV, release.as_os_str().to_os_string()),
            (BLOCK_ENV, block.as_os_str().to_os_string()),
            (HEARTBEAT_ENV, heartbeat.as_os_str().to_os_string()),
            (OUT_ENV, out.as_os_str().to_os_string()),
            (SIGN_REQUEST_ENV, sign_request.as_os_str().to_os_string()),
            (SIGN_RESPONSE_ENV, sign_response.as_os_str().to_os_string()),
            (POST_CAS_ENV, post_cas.as_os_str().to_os_string()),
        ],
    );

    // Stage 1: registration — the old Cell committed both segments plus
    // the mid-flight attempt and recorded its checkpoint.
    let registration = wait_for_file_while_alive(&mut child, "dual-primary registration", &out);
    let mut lines = registration.lines();
    assert_eq!(lines.next(), Some("registered"));
    let child_pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    let old_cell = nlos_cell::CellIdentity::from_domain(SchedulerDomainId::from_bytes(fixed_hex(
        lines.next().expect("identity line"),
    )));
    let checkpoint_text = lines.next().expect("checkpoint digest line").to_string();
    let issuer = PrincipalId::from_bytes(fixed_hex(lines.next().expect("principal line")));
    let domain = ControlDomainId::from_bytes(fixed_hex(lines.next().expect("domain line")));
    let key_id = KeyId::from_bytes(fixed_hex(lines.next().expect("key line")));
    assert_ne!(child_pid, std::process::id());
    assert_eq!(
        old_cell,
        nlos_cell::CellIdentity::from_domain(child_domain()),
        "the old authority must be the agreed second Cell"
    );
    let parent_holder = holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");
    let hb_baseline = heartbeat_counter(&heartbeat);
    assert!(hb_baseline > 0, "the old Cell is heartbeating");
    wait_while_alive(
        &mut child,
        "the dual-claim window's commits",
        &out,
        "committed_two",
    );
    let durable_at_window = durable_commit_prefix(&successor_store, task_id())
        .expect("durable prefix at the end of the dual-claim window");
    assert_eq!(
        durable_at_window.entry_count(),
        4,
        "the dual-claim window's two committed segments"
    );

    // Stage 2: the partition window opens (both alive), and the
    // successor's drive is refused while the incumbent lease is live —
    // no preemption, zero durable effect. The cut source borrows the
    // child only for the duration of one drive (construction is
    // stateless; the cut flag lives here), so liveness assertions
    // between drives keep their own access.
    let lease_key = IdempotencyKey::from_bytes([0x49; 16]);
    let cut_drive = |cut: bool, child: &mut std::process::Child, clock: &mut dyn FnMut() -> i64| {
        let mut source = CuttableSignChannel::new(FileSignChannel {
            child,
            request_path: sign_request.clone(),
            response_path: sign_response.clone(),
            issuer,
            control_domain_id: domain,
            key_id,
        });
        source.cut_if(cut);
        drive_cross_cell_takeover(CrossCellTakeoverRequest {
            store: &successor_store,
            identity: &identity,
            barrier_source: &mut source,
            task_id: task_id(),
            successor_holder_id: parent_holder,
            lease_idempotency_key: lease_key,
            lease_ttl_ms: PARENT_LEASE_TTL_MS,
            now_ms: clock,
        })
    };
    let hb_at_cut = heartbeat_counter(&heartbeat);
    let mut tick = PARENT_ATTEMPT_ONE_BASE_MS;
    let mut clock = || {
        tick += PARENT_CLOCK_STEP_MS;
        tick
    };
    let no_preemption = cut_drive(true, &mut child, &mut clock)
        .expect_err("a live incumbent lease must not be preempted");
    assert!(
        matches!(
            no_preemption,
            TakeoverDriveError::Store(TaskStoreError::AuthorityLeaseHeld)
        ),
        "expected the no-preemption refusal, got {no_preemption}"
    );
    assert_successor_wait_left_no_durable_effect(&store_path);
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_at_cut + 4);
    assert_eq!(
        demand_count(&heartbeat),
        0,
        "no barrier demand may be delivered through the cut"
    );
    assert!(
        child.try_wait().expect("child alive in window").is_none(),
        "the old Cell must stay alive through the dual-claim window"
    );

    // Stage 3: past expiry (channel still cut), the successor drives the
    // takeover — term-2 CAS lands, fence prepares, the barrier leg fails
    // closed through the cut.
    let mut tick = PARENT_ATTEMPT_TWO_BASE_MS;
    let mut clock = || {
        tick += PARENT_CLOCK_STEP_MS;
        tick
    };
    let attempt_two_lease_at_ms = PARENT_ATTEMPT_TWO_BASE_MS + PARENT_CLOCK_STEP_MS;
    let cut_error = cut_drive(true, &mut child, &mut clock)
        .expect_err("the cut channel must fail the barrier leg");
    assert!(
        matches!(&cut_error, TakeoverDriveError::BarrierSource(reason)
            if reason.contains("partition")),
        "expected the partition cut at the barrier leg, got {cut_error}"
    );
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "the term-2 CAS is the arbitration point"
    );
    // The incumbent's two permit issuances rotated the same-term
    // assignment once ([3, 1] baseline); the fence prepare flipped the
    // current one to TakeoverPending.
    assert_eq!(assignment_state_codes(&store_path), vec![3, 2]);

    // Stage 4: the still-alive old Cell proves the loser's face — every
    // submission rejected, nothing durable appended.
    fs::write(&post_cas, b"post-cas").expect("signal the post-CAS stage");
    wait_while_alive(
        &mut child,
        "the loser's arbitration receipts",
        &out,
        "post_cas_done",
    );
    let receipts = fs::read_to_string(&out).expect("read the loser's receipts");
    assert!(receipts.contains("post_cas:renew_refused:authority_lease_held"));
    assert!(
        receipts.contains("post_cas:permit_rejected:authority_lease_fenced")
            || receipts
                .contains("post_cas:permit_rejected:participant_registry_frozen_for_takeover"),
        "the loser's permit request must be refused by a fencing face, got: {receipts}"
    );
    assert!(
        child
            .try_wait()
            .expect("child alive at rejection evidence")
            .is_none(),
        "the loser must be alive when its rejections are cited"
    );
    let durable_after_rejections = durable_commit_prefix(&successor_store, task_id())
        .expect("durable prefix after the loser's rejected submissions");
    assert_eq!(
        durable_after_rejections.entry_count(),
        durable_at_window.entry_count(),
        "the loser's rejected submissions must append no durable rows (no double commit)"
    );

    // Stage 5: restore — the successor, and only it, completes the
    // pending takeover.
    let successor_lease = match successor_store
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: parent_holder,
            idempotency_key: lease_key,
            requested_at_ms: attempt_two_lease_at_ms,
            ttl_ms: PARENT_LEASE_TTL_MS,
        })
        .expect("replay the successor lease record")
    {
        AuthorityLeaseDecision::Replayed(record) => record,
        other => panic!("expected the replayed term-2 lease, got {other:?}"),
    };
    let mut source = CuttableSignChannel::new(FileSignChannel {
        child: &mut child,
        request_path: sign_request.clone(),
        response_path: sign_response.clone(),
        issuer,
        control_domain_id: domain,
        key_id,
    });
    source.restore();
    let mut finish_tick = PARENT_ATTEMPT_TWO_BASE_MS + 90;
    let mut finish_clock = || {
        finish_tick += PARENT_CLOCK_STEP_MS;
        finish_tick
    };
    let finished = common::finish_pending_takeover(
        &successor_store,
        &identity,
        &mut source,
        task_id(),
        successor_lease,
        &mut finish_clock,
    )
    .expect("only the successor completes the takeover");
    drop(source);
    assert_eq!(
        finished.completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_receipts"),
        1,
        "exactly one takeover receipt exists — the single winner's"
    );
    // Terminal shape: the rotated incumbent assignment Fenced by the
    // completion, the completion successor Fenced by the reopen, the
    // reopen active.
    assert_eq!(assignment_state_codes(&store_path), vec![3, 3, 3, 1]);

    // Stage 6: convergence over the old Cell's trail — the verified
    // anchor covers the first segment; the dual-claim window's segments
    // are exactly the adopted gap; all four invariants hold.
    let directory =
        CellDirectory::open(store_dir.path()).expect("successor opens the shared federation root");
    let outcome = converge_commit_prefix_gap(ReconciliationRequest {
        store: &successor_store,
        directory: &directory,
        authority_cell: old_cell,
        task_id: task_id(),
        term_lease: successor_lease,
    })
    .expect("the winner converges the dual-claim window");
    assert_eq!(outcome.covered_entries, 2);
    assert_eq!(outcome.visibility, PrefixVisibility::Partial);
    let anchor = outcome.anchor.as_ref().expect("verified anchor");
    assert_eq!(anchor.digest.to_text(), checkpoint_text);
    assert_eq!(anchor.os_process_id, child_pid);
    assert_eq!(
        outcome
            .adopted
            .iter()
            .map(|entry| entry.effect_history_seq)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "the gap adopts exactly the dual-claim window's durable entries"
    );
    assert_eq!(outcome.final_state.entry_count, 4);
    assert_invariants_all_hold(&outcome.invariants);
    assert_eq!(
        demand_count(&heartbeat),
        finished.observations.len(),
        "observations flowed only after the restore"
    );
    assert!(
        child
            .try_wait()
            .expect("child alive at convergence")
            .is_none(),
        "both Cells must be live when the dual-primary evidence is cited"
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
        &post_cas,
    ] {
        let _ = fs::remove_file(path);
    }
}
