//! CS0-D H6 category **stale epoch**: after the successor's term-2 lease
//! CAS has fenced the old Cell's term-1 authority, the still-live old
//! Cell keeps submitting with its stale token/term — and every
//! injectable action class of the `DIST-TASK-002` tail sentence is
//! rejected by the lease/fencing machinery.
//!
//! Scenario shape (the old Cell process stays alive throughout — the
//! fencing is durable, not process death):
//!
//! 1. the old Cell holds term 1, commits its baseline segment, and opens
//!    one mid-flight attempt under its live lease (slot 0 closed, slot 1
//!    dispatched with its outcome pending, slot 2 declared but never
//!    effect-permitted);
//! 2. the successor drives past the incumbent expiry: term-2 CAS +
//!    fence prepare land, and the drive then fail-closes with
//!    `FenceManifestUnavailable` — in this store shape an outstanding
//!    planned-effects permit without sealed effect endpoints leaves the
//!    exact fence set unresolvable, so the takeover cannot *complete*
//!    while the mid-flight permit is outstanding (an honest fail-closed
//!    in its own right, and precisely why this category holds a
//!    mid-flight permit: the fence's refusal face is exposed);
//! 3. the still-live old Cell then submits under its stale term-1 lease
//!    across every injectable class, and each is refused:
//!    - `fence`  → `prepare_authority_takeover_fence`:
//!      `AuthorityLeaseFenced` (lease CAS);
//!    - `permit` → two faces, because the store runs the
//!      `TASK-COMMIT-001` compete CAS (one outstanding `Issued` permit
//!      per task) *ahead of* the issuance-time lease gate: (a) a fresh
//!      lease-bound `CommitPermit` request on the fenced task loses the
//!      CAS and is typed `Ok(PermitDecision::Superseded { winner })` —
//!      no permit is issued, the requesting attempt is durably fenced
//!      terminal (`AttemptState::Superseded`), and the winner is
//!      asserted to be the pre-existing outstanding mid-flight permit,
//!      never a new grant to the old authority; (b) a stale-lease
//!      permit request whose compete CAS wins (a freshly registered
//!      task with no outstanding permit) reaches the issuance gate and
//!      is refused by `AuthorityLeaseFenced` — the stale token itself
//!      is fenced, so the permit class is not refused by the
//!      outstanding-permit rule alone;
//!    - `effect` → an `EffectPermit` request on the issued permit:
//!      `ParticipantRegistryFrozen { FrozenForTakeover }` (the live
//!      frozen-registry fence that fires ahead of the lease check — the
//!      same fencing arbitration, typed by the registry face);
//!    - `result` → `record_effect_outcome` on the dispatched slot: the
//!      same frozen-registry refusal;
//!    - `commit` → `finalize_commit_v3` on the issued permit:
//!      `AuthorityLeaseFenced` (the finalize gate validates the lease
//!      binding before its terminal-state machinery);
//! 4. every rejected submission leaves the durable commit prefix
//!    unchanged (the `Superseded` face durably closes the *requesting*
//!    attempt — a refusal outcome, not a canonical effect granted to the
//!    old authority).
//!
//! `message` / `callback` have no `TaskStore`-endpoint face in this
//! slice — declared not injectable here, not claimed (the six-type
//! endpoint mapping is Phase 1 CS1-C work).
//!
//! **Evidence scope (honesty bound):** single-host, dual-process
//! evidence — shared filesystem, shared kernel, shared clock domain.
//! Citing it as cross-host / network-partition / cross-clock evidence
//! would violate RISK-B-11. Endpoint coverage declared: the `TaskStore`
//! participant endpoint only.

mod common;

use std::fs;
use std::path::Path;

use common::{
    CuttableSignChannel, FileSignChannel, MidFlightAttempts, acquire_incumbent_lease, append_line,
    assignment_state_codes, bootstrap_barrier_signer, commit_segment, fixed_hex, heartbeat_counter,
    hex, holder_from_pid, lease_history_rows, open_mid_flight_attempts, publish, raw_count,
    spawn_fault_child, spawn_heartbeat, wait_for_file_while_alive, wait_heartbeat_at_least,
    wait_while_alive,
};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{
    CrossCellTakeoverRequest, TakeoverDriveError, drive_cross_cell_takeover, durable_commit_prefix,
};
use nlos_task::{
    AuthorityLeasePermitRequest, AuthorityLeaseTakeoverFenceRequest, FinalizeRequest,
    FinalizeRequestV3, PermitDecision, PermitRequest, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{ControlDomainId, IdempotencyKey, KeyId, PrincipalId, TaskId};

const STORE_DIR_ENV: &str = "NLOS_CS0D_STALE_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_STALE_RELEASE";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_STALE_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_STALE_OUT";
const STALE_ENV: &str = "NLOS_CS0D_STALE_SUBMIT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_STALE_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_STALE_RESP";

const TASK_SEED: u8 = 0xE1;
/// A second task registered after the fence, used by the permit-class
/// face (b): it holds no outstanding permit, so the stale-lease request
/// wins the compete CAS and actually reaches the issuance-time lease
/// gate (whose refusal is the fence face of the permit class).
const TASK_UNFENCED_SEED: u8 = 0xE2;
const CHILD_SEED: u8 = 0xD1;
const CHILD_HOLDER_TAG: u8 = 0xC2;
const PARENT_HOLDER_TAG: u8 = 0xC3;
const SIGNER_SEED: u8 = 0xAF;

/// Logical clock: the incumbent term-1 lease at 100 with TTL 100; the
/// successor drives at 300+ (past expiry); the old Cell's stale
/// submissions run at 310+.
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const BASELINE_AT_MS: i64 = 110;
const MID_FLIGHT_AT_MS: i64 = 120;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_CLOCK_BASE_MS: i64 = 295;
const PARENT_CLOCK_STEP_MS: i64 = 10;
const CHILD_STALE_AT_MS: i64 = 310;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

/// Harness-only entry: Cell A, the old authority. Commits its baseline
/// segment, opens the mid-flight attempt under its live lease,
/// bootstraps the shared signer, then — once the successor's term-2 CAS
/// has landed — proves every injectable stale-epoch submission is
/// refused, staying alive throughout.
#[test]
#[ignore = "spawned by stale_epoch_submissions_are_all_refused"]
#[allow(clippy::too_many_lines)]
fn child_stale_old_cell_helper() {
    // The sign-channel paths are bound (the parent spawns this helper
    // with them) but never used: this category's drive fail-closes
    // before any barrier demand, so the old Cell runs no sign server.
    let (
        Ok(store_dir),
        Ok(release),
        Ok(heartbeat),
        Ok(out),
        Ok(stale),
        Ok(_sign_request),
        Ok(_sign_response),
    ) = (
        std::env::var(STORE_DIR_ENV),
        std::env::var(RELEASE_ENV),
        std::env::var(HEARTBEAT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(STALE_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
    )
    else {
        eprintln!("skipped: cs0d stale child env unset (harness entry, parent spawn only)");
        return;
    };
    let authority = SqliteTaskAuthority::open(Path::new(&store_dir).join("task-authority.sqlite3"))
        .expect("old Cell opens its task store");
    let lease_one = acquire_incumbent_lease(
        &authority,
        holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
        CHILD_SEED,
        LEASE_ONE_AT_MS,
        LEASE_ONE_TTL_MS,
    );
    assert_eq!(lease_one.term, 1);
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x2F,
        0,
        BASELINE_AT_MS,
        2,
    );
    let mid_flight: MidFlightAttempts = open_mid_flight_attempts(
        &authority,
        &lease_one,
        task_id(),
        0x30,
        16,
        MID_FLIGHT_AT_MS,
    );
    let heartbeat_thread = spawn_heartbeat(Path::new(&heartbeat), Path::new(&release));
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

    // Wait for the successor's term-2 CAS, then submit under the stale
    // term-1 lease across every injectable class.
    let deadline = std::time::Instant::now() + common::CHILD_WAIT;
    while !Path::new(&stale).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "old Cell timed out waiting for the stale-submission stage"
        );
        std::thread::sleep(common::POLL);
    }

    // fence class: a stale-lease takeover-fence prepare.
    let registry_binding = authority
        .inspect_authority_assignment(task_id())
        .expect("assignment read")
        .participant_registry_binding;
    let prepare_refused = matches!(
        authority.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_one,
            requested_at_ms: CHILD_STALE_AT_MS,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    );
    assert!(prepare_refused, "the stale fence prepare must be refused");
    append_line(
        Path::new(&out),
        "stale:fence_prepare:authority_lease_fenced",
    );

    // permit class, face (a): a fresh lease-bound CommitPermit request on
    // the fenced task. The TASK-COMMIT-001 compete CAS (one outstanding
    // `Issued` permit per task) runs ahead of the issuance-time lease
    // gate, so the typed refusal here is the CAS loss: nothing is issued
    // and the requesting attempt is durably fenced terminal.
    let fresh_attempt =
        common::register_task_and_attempt(&authority, task_id(), 0x31, CHILD_STALE_AT_MS + 1);
    match authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
        permit: PermitRequest {
            task_id: task_id(),
            attempt_id: fresh_attempt.attempt_id,
            attempt_generation: fresh_attempt.attempt_generation,
            write_set_root: [0x31; 32],
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x32; 16]),
            valid_until_ms: 1_000_000,
            requested_at_ms: CHILD_STALE_AT_MS + 2,
        },
        lease: lease_one,
    }) {
        Err(TaskStoreError::AuthorityLeaseFenced) => {
            append_line(Path::new(&out), "stale:permit:authority_lease_fenced");
        }
        Err(TaskStoreError::ParticipantRegistryFrozen { state }) => {
            assert_eq!(
                state,
                nlos_task::ParticipantRegistryState::FrozenForTakeover,
                "the frozen refusal must name the takeover freeze"
            );
            append_line(
                Path::new(&out),
                "stale:permit:participant_registry_frozen_for_takeover",
            );
        }
        Ok(PermitDecision::Superseded { winner }) => {
            // Semantic refusal, typed by the compete CAS: no permit was
            // issued to the old authority and the fresh attempt is
            // durably fenced terminal. The winner must be the
            // pre-existing outstanding mid-flight permit — a new grant
            // to the stale epoch would be a fencing failure.
            assert_ne!(
                winner.attempt_id, fresh_attempt.attempt_id,
                "the CAS winner must not be the stale request itself"
            );
            assert_eq!(
                winner.permit_id, mid_flight.flighted_permit.permit_id,
                "the CAS winner must be the pre-existing mid-flight permit"
            );
            append_line(
                Path::new(&out),
                "stale:permit:superseded_by_outstanding_permit",
            );
        }
        other => panic!("the stale permit request must be refused, got {other:?}"),
    }

    // permit class, face (b): the stale-lease permit request whose
    // compete CAS wins (a freshly registered task holds no outstanding
    // permit) reaches the issuance-time lease gate — the stale term-1
    // token itself is fenced there, proving the permit class is not
    // refused by the outstanding-permit rule alone.
    let unfenced_task = TaskId::from_bytes([TASK_UNFENCED_SEED; 16]);
    let unfenced_attempt =
        common::register_task_and_attempt(&authority, unfenced_task, 0x36, CHILD_STALE_AT_MS + 6);
    match authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
        permit: PermitRequest {
            task_id: unfenced_task,
            attempt_id: unfenced_attempt.attempt_id,
            attempt_generation: unfenced_attempt.attempt_generation,
            write_set_root: [0x36; 32],
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x37; 16]),
            valid_until_ms: 1_000_000,
            requested_at_ms: CHILD_STALE_AT_MS + 7,
        },
        lease: lease_one,
    }) {
        Err(TaskStoreError::AuthorityLeaseFenced) => {
            append_line(
                Path::new(&out),
                "stale:permit_fence_gate:authority_lease_fenced",
            );
        }
        other => {
            panic!("the stale lease must be fenced at the permit issuance gate, got {other:?}")
        }
    }

    // effect class: an EffectPermit request on the issued permit. The
    // frozen-registry fence fires ahead of the lease check here — the
    // same fencing arbitration, typed by the registry face.
    match authority.request_effect_permit_with_authority_lease(
        nlos_task::AuthorityLeaseEffectPermitRequest {
            permit: nlos_task::EffectPermitRequest {
                task_id: task_id(),
                attempt_id: mid_flight.flighted.attempt_id,
                attempt_generation: mid_flight.flighted.attempt_generation,
                permit_id: mid_flight.flighted_permit.permit_id,
                permit_epoch: mid_flight.flighted_permit.permit_epoch,
                effect_seq: mid_flight.unrequested_effect_slot,
                idempotency_key: IdempotencyKey::from_bytes([0x33; 16]),
                valid_until_ms: 1_000_000,
                requested_at_ms: CHILD_STALE_AT_MS + 3,
            },
            lease: lease_one,
        },
    ) {
        Err(TaskStoreError::ParticipantRegistryFrozen { state }) => {
            assert_eq!(
                state,
                nlos_task::ParticipantRegistryState::FrozenForTakeover,
                "the frozen refusal must name the takeover freeze"
            );
            append_line(
                Path::new(&out),
                "stale:effect:participant_registry_frozen_for_takeover",
            );
        }
        Err(TaskStoreError::AuthorityLeaseFenced) => {
            append_line(Path::new(&out), "stale:effect:authority_lease_fenced");
        }
        other => panic!("the stale effect permit must be refused, got {other:?}"),
    }

    // result class: the outcome of the dispatched mid-flight slot.
    match authority.record_effect_outcome_with_authority_lease(
        nlos_task::AuthorityLeaseOutcomeRequest {
            outcome: nlos_task::OutcomeRequest {
                task_id: task_id(),
                attempt_id: mid_flight.flighted.attempt_id,
                attempt_generation: mid_flight.flighted.attempt_generation,
                permit_id: mid_flight.flighted_permit.permit_id,
                permit_epoch: mid_flight.flighted_permit.permit_epoch,
                effect_seq: mid_flight.pending_outcome_slot,
                outcome: nlos_task::Outcome::Closed {
                    authoritative_closure_digest: [0x34; 32],
                },
                recorded_at_ms: CHILD_STALE_AT_MS + 4,
            },
            lease: lease_one,
        },
    ) {
        Err(TaskStoreError::ParticipantRegistryFrozen { state }) => {
            assert_eq!(
                state,
                nlos_task::ParticipantRegistryState::FrozenForTakeover,
                "the frozen refusal must name the takeover freeze"
            );
            append_line(
                Path::new(&out),
                "stale:result:participant_registry_frozen_for_takeover",
            );
        }
        Err(TaskStoreError::AuthorityLeaseFenced) => {
            append_line(Path::new(&out), "stale:result:authority_lease_fenced");
        }
        other => panic!("the stale outcome must be refused, got {other:?}"),
    }

    // commit class: the finalize of the issued permit. The finalize gate
    // validates the lease binding before its terminal-state machinery.
    let finalize_refused = matches!(
        authority.finalize_commit_v3_with_authority_lease(
            nlos_task::AuthorityLeaseFinalizeRequest {
                finalize: FinalizeRequestV3 {
                    base: FinalizeRequest {
                        task_id: task_id(),
                        attempt_id: mid_flight.flighted.attempt_id,
                        attempt_generation: mid_flight.flighted.attempt_generation,
                        permit_id: mid_flight.flighted_permit.permit_id,
                        new_effect_history_root: [0; 32],
                        new_retry_fence_epoch: 0,
                        finalized_at_ms: CHILD_STALE_AT_MS + 5,
                    },
                    required_satisfaction: Vec::new(),
                    fenced_participant_digest: [0; 32],
                },
                lease: lease_one,
            }
        ),
        Err(TaskStoreError::AuthorityLeaseFenced)
    );
    assert!(finalize_refused, "the stale finalize must be refused");
    append_line(Path::new(&out), "stale:commit:authority_lease_fenced");
    append_line(Path::new(&out), "stale_done");

    // The stale epoch's submissions are done; stay alive until released.
    let deadline = std::time::Instant::now() + common::CHILD_WAIT;
    while !Path::new(&release).exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "old Cell timed out waiting for release"
        );
        std::thread::sleep(common::POLL);
    }
    heartbeat_thread.join().expect("heartbeat thread");
}

/// The stale-epoch category: after the term-2 CAS, every injectable
/// stale-token submission of the still-live old Cell is refused, and the
/// refusals leave the durable prefix untouched.
#[test]
#[allow(clippy::too_many_lines)]
fn stale_epoch_submissions_are_all_refused() {
    let store_dir = common::TempDir::new("stale-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-stale-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-stale-hb-{stamp}.txt"));
    let stale = base.join(format!("nlos-slice-k-cs0d-stale-submit-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-stale-release-{stamp}.flag"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-stale-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-stale-resp-{stamp}.txt"));
    for path in [
        &out,
        &heartbeat,
        &stale,
        &release,
        &sign_request,
        &sign_response,
    ] {
        let _ = fs::remove_file(path);
    }

    let mut child = spawn_fault_child(
        "child_stale_old_cell_helper",
        &[
            (STORE_DIR_ENV, store_dir.path().as_os_str().to_os_string()),
            (RELEASE_ENV, release.as_os_str().to_os_string()),
            (HEARTBEAT_ENV, heartbeat.as_os_str().to_os_string()),
            (OUT_ENV, out.as_os_str().to_os_string()),
            (STALE_ENV, stale.as_os_str().to_os_string()),
            (SIGN_REQUEST_ENV, sign_request.as_os_str().to_os_string()),
            (SIGN_RESPONSE_ENV, sign_response.as_os_str().to_os_string()),
        ],
    );

    // Stage 1: registration — the old Cell holds term 1, its baseline
    // segment, and the mid-flight attempt.
    let registration = wait_for_file_while_alive(&mut child, "stale child registration", &out);
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
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");
    let durable_before = durable_commit_prefix(&successor_store, task_id())
        .expect("durable prefix before the drive");
    assert_eq!(
        durable_before.entry_count(),
        3,
        "the baseline segment (2) plus the mid-flight attempt's closed slot"
    );

    // Stage 2: the successor's drive. Term-2 CAS and fence prepare land;
    // the drive then fail-closes at the unresolvable exact fence set —
    // the outstanding planned-effects permit without sealed endpoints
    // keeps the takeover incomplete, which is exactly the window the
    // stale-epoch injections need (fenced lease, frozen registry).
    let mut source = CuttableSignChannel::new(FileSignChannel {
        child: &mut child,
        request_path: sign_request.clone(),
        response_path: sign_response.clone(),
        issuer,
        control_domain_id: domain,
        key_id,
    });
    let mut tick = PARENT_CLOCK_BASE_MS;
    let mut clock = || {
        tick += PARENT_CLOCK_STEP_MS;
        tick
    };
    let drive_error = drive_cross_cell_takeover(CrossCellTakeoverRequest {
        store: &successor_store,
        identity: &identity,
        barrier_source: &mut source,
        task_id: task_id(),
        successor_holder_id: common::holder_from_pid(std::process::id(), PARENT_HOLDER_TAG),
        lease_idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
        lease_ttl_ms: PARENT_LEASE_TTL_MS,
        now_ms: &mut clock,
    })
    .expect_err("the drive must fail closed on the unresolvable fence set");
    assert!(
        matches!(drive_error, TakeoverDriveError::FenceManifestUnavailable),
        "expected the unresolvable-manifest fail-closed refusal, got {drive_error}"
    );
    drop(source);
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "the term-2 CAS fenced the old Cell's lease"
    );
    // The baseline permit and the mid-flight permit rotated the
    // same-term assignment once ([3, 1] baseline); the fence prepare
    // flipped the current one to TakeoverPending.
    assert_eq!(assignment_state_codes(&store_path), vec![3, 2]);
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_barrier_receipts"),
        0,
        "no barrier observation was ever demanded"
    );
    assert_eq!(
        common::demand_count(&heartbeat),
        0,
        "the old Cell received no barrier demand"
    );

    // Stage 3: the still-live old Cell submits under the stale term-1
    // lease — every injectable class must be refused.
    fs::write(&stale, b"stale").expect("signal the stale-submission stage");
    wait_while_alive(
        &mut child,
        "the stale-submission receipts",
        &out,
        "stale_done",
    );
    let receipts = fs::read_to_string(&out).expect("read the stale receipts");
    assert!(receipts.contains("stale:fence_prepare:authority_lease_fenced"));
    assert!(
        receipts.contains("stale:permit:authority_lease_fenced")
            || receipts.contains("stale:permit:participant_registry_frozen_for_takeover")
            || receipts.contains("stale:permit:superseded_by_outstanding_permit"),
        "the stale permit must be refused without issuing to the old authority: {receipts}"
    );
    assert!(
        receipts.contains("stale:permit_fence_gate:authority_lease_fenced"),
        "the stale lease itself must be fenced at the permit issuance gate: {receipts}"
    );
    assert!(
        receipts.contains("stale:effect:participant_registry_frozen_for_takeover")
            || receipts.contains("stale:effect:authority_lease_fenced"),
        "the stale effect permit must be refused by a fencing face: {receipts}"
    );
    assert!(
        receipts.contains("stale:result:participant_registry_frozen_for_takeover")
            || receipts.contains("stale:result:authority_lease_fenced"),
        "the stale outcome must be refused by a fencing face: {receipts}"
    );
    assert!(receipts.contains("stale:commit:authority_lease_fenced"));
    assert!(
        child.try_wait().expect("child alive at refusals").is_none(),
        "the old Cell must be alive when its stale refusals are cited"
    );

    // Stage 4: the refusals appended nothing — the durable prefix and
    // the takeover tables are untouched by the stale epoch.
    let durable_after = durable_commit_prefix(&successor_store, task_id())
        .expect("durable prefix after the stale submissions");
    assert_eq!(
        durable_after.entry_count(),
        durable_before.entry_count(),
        "the stale submissions must append no durable rows"
    );
    assert_eq!(
        durable_after.durable_prefix_root(),
        durable_before.durable_prefix_root()
    );
    wait_heartbeat_at_least(&mut child, &heartbeat, heartbeat_counter(&heartbeat) + 2);
    assert!(
        child
            .try_wait()
            .expect("child alive at prefix evidence")
            .is_none(),
        "both Cells must be live when the stale-epoch evidence is cited"
    );

    fs::write(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "old Cell process failed: {status:?}");
    for path in [
        &out,
        &heartbeat,
        &stale,
        &release,
        &sign_request,
        &sign_response,
    ] {
        let _ = fs::remove_file(path);
    }
}
