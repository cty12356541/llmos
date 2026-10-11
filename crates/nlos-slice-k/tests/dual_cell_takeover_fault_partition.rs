//! CS0-D H6 category **partition**: with BOTH OS processes provably alive
//! (ADR-0018: one process = one Cell), the successor's barrier-observation
//! channel to the old Cell is cut, and the takeover semantics fail closed
//! exactly as `DIST-TASK-002` / ADR-0019 R1.2.4 require.
//!
//! Assertion semantics (per the lane brief and R1.2.4):
//!
//! 1. **No preemption — wait for expiry**: while the incumbent term-1
//!    lease is live, the successor's drive is refused at the anchored
//!    lease gate with `AuthorityLeaseHeld` (the drive's
//!    `acquire_authority_lease_anchored` fail-closed judgement — the
//!    successor cannot widen the window in which a live lease is taken
//!    over). The refusal leaves **zero durable takeover effect** — the
//!    R1.2.4 quarantine face available at this slice's declared endpoint
//!    (`TaskStore`): while waiting, the successor's outstanding work (the
//!    drive itself) writes nothing. The full `QUARANTINED` outstanding-
//!    effect lifecycle belongs to the resource-commit machinery outside
//!    this slice's coverage and is not claimed here.
//! 2. **No completion through the gap**: after the incumbent lease
//!    expires (logical clock advance — the store holds no clock by
//!    convention), the successor legally acquires term 2 and prepares the
//!    fence, but with the channel cut the barrier leg fails closed
//!    (`TakeoverDriveError::BarrierSource`), the takeover receipt stays
//!    `Pending`, coverage stays incomplete, the old assignment stays
//!    `TakeoverPending`, and nothing completes or reopens.
//! 3. **Both-Cells-alive proof through every cut window**: the old Cell's
//!    heartbeat counter (a file outside the blocked channel) advances
//!    past each injection point, its served-demand counter shows it
//!    received nothing, and `try_wait` reports it alive at every
//!    evidence point. Channel cut ≠ process death (kill would only prove
//!    crash handling — R2.2 item 3 methodological constraint).
//! 4. **Recovery**: once the channel is restored, the pending takeover
//!    completes through the store's own idempotent gates (the drive
//!    itself is one-shot per receipt — its re-entry guard refusal is
//!    asserted too) with the same final readbacks the fault-free drive
//!    asserts.
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. The injected
//! "cut" is a delivery refusal at the harness channel, not an OS-level
//! network partition; citing it as cross-host / network-partition /
//! cross-clock evidence would violate RISK-B-11. Endpoint coverage
//! declared: the `TaskStore` participant endpoint only (the six-type
//! mapping is Phase 1 CS1-C work).

mod common;

use std::fs;
use std::path::Path;

use common::{
    CuttableSignChannel, FileSignChannel, acquire_incumbent_lease, append_line,
    assert_successor_wait_left_no_durable_effect, assignment_state_codes, bootstrap_barrier_signer,
    commit_segment, demand_count, fixed_hex, heartbeat_counter, hex, lease_history_rows, publish,
    raw_count, spawn_fault_child, spawn_heartbeat, wait_for_file_while_alive,
    wait_heartbeat_at_least,
};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{CrossCellTakeoverRequest, TakeoverDriveError, drive_cross_cell_takeover};
use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, AuthorityTakeoverReceiptState,
    ParticipantRegistryState, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{ControlDomainId, IdempotencyKey, KeyId, PrincipalId, TaskId};

const STORE_DIR_ENV: &str = "NLOS_CS0D_PART_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_PART_RELEASE";
const BLOCK_ENV: &str = "NLOS_CS0D_PART_BLOCK";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_PART_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_PART_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_PART_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_PART_RESP";

const TASK_SEED: u8 = 0xD7;
const CHILD_SEED: u8 = 0xC7;
const CHILD_HOLDER_TAG: u8 = 0xB8;
const PARENT_HOLDER_TAG: u8 = 0xB9;
const SIGNER_SEED: u8 = 0xA8;

/// Logical clock: the old Cell's term-1 lease runs at 100 with TTL 100
/// (expires at 200); its baseline commit runs inside that window; the
/// successor's first attempt runs at ~150 (still live — the no-preemption
/// wait), its second attempt at 300+ (past expiry — the legal takeover
/// window with the channel cut).
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_ATTEMPT_ONE_BASE_MS: i64 = 140;
const PARENT_ATTEMPT_TWO_BASE_MS: i64 = 295;
const PARENT_CLOCK_STEP_MS: i64 = 10;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

/// Harness-only entry: Cell A, the old authority. Acquires the term-1
/// lease, commits its baseline prefix (which establishes the participant
/// registry baseline), bootstraps the shared barrier signer, then serves
/// signed barrier observations (block-flag aware) until released,
/// heartbeating throughout.
#[test]
#[ignore = "spawned by partition_cut_blocks_takeover_until_channel_recovers"]
fn child_partition_old_cell_helper() {
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
        eprintln!("skipped: cs0d partition child env unset (harness entry, parent spawn only)");
        return;
    };
    let authority = SqliteTaskAuthority::open(Path::new(&store_dir).join("task-authority.sqlite3"))
        .expect("old Cell opens its task store");
    let lease_one = acquire_incumbent_lease(
        &authority,
        common::holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
        CHILD_SEED,
        LEASE_ONE_AT_MS,
        LEASE_ONE_TTL_MS,
    );
    assert_eq!(lease_one.term, 1);
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x27,
        0,
        LEASE_ONE_AT_MS + 10,
        2,
    );
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
        &ed25519_dalek::SigningKey::from_bytes(&[SIGNER_SEED; 32]),
    );
    append_line(Path::new(&out), "serving_done");
    heartbeat_thread.join().expect("heartbeat thread");
}

/// The partition category: with both Cells alive, cutting the barrier
/// observation channel prevents the takeover from completing through the
/// gap (no preemption while the incumbent lease is live; barrier
/// fail-closed after expiry), and restoring the channel lets the pending
/// takeover complete.
#[test]
#[allow(clippy::too_many_lines)]
fn partition_cut_blocks_takeover_until_channel_recovers() {
    let store_dir = common::TempDir::new("partition-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-part-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-part-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-part-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-part-resp-{stamp}.txt"));
    let block = base.join(format!("nlos-slice-k-cs0d-part-block-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-part-release-{stamp}.flag"));
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
        "child_partition_old_cell_helper",
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

    // Stage 1: the old Cell holds term 1 and its baseline prefix.
    let registration = wait_for_file_while_alive(&mut child, "partition child registration", &out);
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
    let child_holder = common::holder_from_pid(child_pid, CHILD_HOLDER_TAG);
    let parent_holder = common::holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");
    let pre_lease = successor_store
        .inspect_authority_lease()
        .expect("read the incumbent lease");
    assert_eq!(pre_lease.term, 1);
    assert_eq!(pre_lease.holder_id, child_holder);
    let hb_baseline = heartbeat_counter(&heartbeat);
    assert!(hb_baseline > 0, "the old Cell is heartbeating");

    // Stage 2: attempt 1 while the incumbent lease is LIVE — the
    // no-preemption wait (R1.2.4). The anchored lease gate refuses; the
    // refusal must leave zero durable takeover effect (quarantine face).
    let lease_key = IdempotencyKey::from_bytes([0x47; 16]);
    {
        let mut source = CuttableSignChannel::new(FileSignChannel {
            child: &mut child,
            request_path: sign_request.clone(),
            response_path: sign_response.clone(),
            issuer,
            control_domain_id: domain,
            key_id,
        });
        let mut tick = PARENT_ATTEMPT_ONE_BASE_MS;
        let mut clock = || {
            tick += PARENT_CLOCK_STEP_MS;
            tick
        };
        let error = drive_cross_cell_takeover(CrossCellTakeoverRequest {
            store: &successor_store,
            identity: &identity,
            barrier_source: &mut source,
            task_id: task_id(),
            successor_holder_id: parent_holder,
            lease_idempotency_key: lease_key,
            lease_ttl_ms: PARENT_LEASE_TTL_MS,
            now_ms: &mut clock,
        })
        .expect_err("a live incumbent lease must not be preempted");
        assert!(
            matches!(
                error,
                TakeoverDriveError::Store(TaskStoreError::AuthorityLeaseHeld)
            ),
            "expected the no-preemption refusal, got {error}"
        );
    }
    assert_successor_wait_left_no_durable_effect(&store_path);
    assert_eq!(
        demand_count(&heartbeat),
        0,
        "no barrier demand may have been delivered while the incumbent is live"
    );
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_baseline + 4);
    assert!(
        child.try_wait().expect("child alive after wait").is_none(),
        "the old Cell must stay alive through the no-preemption window"
    );

    // Stage 3: attempt 2 past expiry with the channel CUT — the legal
    // takeover window in which the gap cannot be completed. Both Cells
    // stay alive; the barrier leg fails closed; the receipt stays
    // Pending with incomplete coverage.
    let mut source = CuttableSignChannel::new(FileSignChannel {
        child: &mut child,
        request_path: sign_request.clone(),
        response_path: sign_response.clone(),
        issuer,
        control_domain_id: domain,
        key_id,
    });
    source.cut();
    let hb_at_cut = heartbeat_counter(&heartbeat);
    let mut tick = PARENT_ATTEMPT_TWO_BASE_MS;
    let mut clock = || {
        tick += PARENT_CLOCK_STEP_MS;
        tick
    };
    let attempt_two_lease_at_ms = PARENT_ATTEMPT_TWO_BASE_MS + PARENT_CLOCK_STEP_MS;
    let cut_error = drive_cross_cell_takeover(CrossCellTakeoverRequest {
        store: &successor_store,
        identity: &IdentityAuthority::open(store_dir.path().join("identity"))
            .expect("successor opens identity root"),
        barrier_source: &mut source,
        task_id: task_id(),
        successor_holder_id: parent_holder,
        lease_idempotency_key: lease_key,
        lease_ttl_ms: PARENT_LEASE_TTL_MS,
        now_ms: &mut clock,
    })
    .expect_err("a cut channel must fail the drive closed at the barrier leg");
    assert!(
        matches!(&cut_error, TakeoverDriveError::BarrierSource(reason)
            if reason.contains("partition")),
        "expected the partition cut at the barrier leg, got {cut_error}"
    );
    drop(source);
    // The old Cell received nothing (demand counter unmoved) while both
    // Cells stayed alive (heartbeat advanced past several beats).
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_at_cut + 4);
    assert_eq!(
        demand_count(&heartbeat),
        0,
        "no barrier demand may be delivered while the channel is cut"
    );
    assert!(
        child.try_wait().expect("child alive during cut").is_none(),
        "the old Cell must stay alive through the cut window"
    );
    // Durable mid-partition state: term 2 acquired (legal, past expiry),
    // fence prepared, receipt Pending, nothing observed/completed.
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "term 2 was legally taken over after expiry"
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_receipts"),
        1
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_barrier_receipts"),
        0,
        "no observation through the cut"
    );
    assert_eq!(
        assignment_state_codes(&store_path),
        vec![2],
        "the old assignment is TakeoverPending, not fenced-complete"
    );
    let frozen = successor_store
        .inspect_participant_registry(task_id())
        .expect("registry mid-partition");
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    let frozen_binding = nlos_task::ParticipantRegistryBinding {
        generation: frozen.generation,
        root: frozen.root,
    };
    let fence_receipt = successor_store
        .inspect_authority_takeover_fence_receipt(task_id(), frozen_binding)
        .expect("fence receipt mid-partition");
    let pending = successor_store
        .inspect_authority_takeover_receipt(task_id(), fence_receipt.receipt_id)
        .expect("pending receipt mid-partition");
    assert_eq!(
        pending.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(pending.new_assignment_id, None);
    let members = successor_store
        .inspect_authority_takeover_fence_members(task_id(), frozen_binding)
        .expect("manifest mid-partition");
    assert_ne!(
        members,
        [] as [nlos_task::AuthorityTakeoverFenceMemberRecord; 0],
        "the frozen manifest names its members"
    );
    let coverage = successor_store
        .inspect_authority_takeover_barrier_coverage(pending.receipt_id)
        .expect("coverage mid-partition");
    assert_ne!(
        coverage.state,
        nlos_task::AuthorityTakeoverBarrierCoverageState::LocallyCovered,
        "coverage cannot be complete through the cut"
    );
    assert_eq!(
        coverage.missing_participants.len(),
        members.len(),
        "every manifest member is still missing through the cut"
    );

    // The drive itself is one-shot per receipt: its re-entry guard
    // refuses the now-pending assignment. Resumption is the store's own
    // idempotent gates (below).
    {
        let mut source = CuttableSignChannel::new(FileSignChannel {
            child: &mut child,
            request_path: sign_request.clone(),
            response_path: sign_response.clone(),
            issuer,
            control_domain_id: domain,
            key_id,
        });
        // The lease leg must replay attempt 2's exact lease bytes (same
        // idempotency key AND same first clock draw of 300), so the
        // refusal below can only come from the drive's own re-entry
        // guard, not a lease-key conflict.
        let mut tick = PARENT_ATTEMPT_TWO_BASE_MS;
        let mut clock = || {
            tick += PARENT_CLOCK_STEP_MS;
            tick
        };
        let refused = drive_cross_cell_takeover(CrossCellTakeoverRequest {
            store: &successor_store,
            identity: &identity,
            barrier_source: &mut source,
            task_id: task_id(),
            successor_holder_id: parent_holder,
            lease_idempotency_key: lease_key,
            lease_ttl_ms: PARENT_LEASE_TTL_MS,
            now_ms: &mut clock,
        })
        .expect_err("the drive must not re-enter a pending takeover");
        assert!(
            matches!(
                refused,
                TakeoverDriveError::OldAssignmentNotActive {
                    state: nlos_task::AuthorityAssignmentState::TakeoverPending
                }
            ),
            "expected the one-shot re-entry refusal, got {refused}"
        );
    }

    // Stage 4: RESTORE the channel — the pending takeover completes
    // through the store's own gates, with the drive's final readbacks.
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
    assert_eq!(successor_lease.term, 2);
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
    .expect("the pending takeover completes once the channel is restored");
    drop(source);
    assert_eq!(
        finished.completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(
        assignment_state_codes(&store_path),
        vec![3, 3, 1],
        "fault-free terminal shape: old and completion assignments fenced, reopen active"
    );
    assert_eq!(finished.registry.state, ParticipantRegistryState::Open);
    assert_eq!(
        finished.registry.prior_root, frozen_binding.root,
        "the reopened generation chains to the frozen root"
    );
    assert_eq!(
        finished.active_assignment.authority_lease_binding,
        successor_lease.binding()
    );
    assert_eq!(
        demand_count(&heartbeat),
        finished.observations.len(),
        "every observation was delivered only after the restore"
    );
    assert!(
        child
            .try_wait()
            .expect("child alive at completion")
            .is_none(),
        "both Cells must be live when the recovery evidence is cited"
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
