//! CS0-D H6 category **duplicate messages**: barrier observations and
//! takeover receipts replayed through the durable gates are idempotent —
//! a redelivered message never produces a second durable effect and
//! never double-commits (`DIST-TASK-002` receipt semantics).
//!
//! Injection shape (both OS processes alive throughout; no fault is
//! injected into any process — the duplicates are message redeliveries
//! by the successor):
//!
//! 1. the old Cell (child) holds the term-1 lease, commits its baseline
//!    prefix, bootstraps the shared barrier signer, and serves barrier
//!    observations;
//! 2. the successor (parent, past the incumbent expiry) acquires the
//!    term-2 lease and prepares the fence through the store's own gates;
//! 3. **channel redelivery**: the successor demands the same observation
//!    twice — the old Cell's endpoint derivation is deterministic, so
//!    both deliveries carry byte-identical material;
//! 4. **gate replay**: the signed-observation gate is fed the same bytes
//!    twice — the second submission returns the byte-equal stored record
//!    and the durable barrier table still holds exactly one row;
//! 5. **completion replay**: `complete_authority_takeover` of the
//!    already-complete receipt re-reads the durable state and returns
//!    the byte-equal record — the assignment table does not grow, the
//!    receipt row does not change;
//! 6. **lease replay**: re-acquiring the term-2 lease with the identical
//!    request replays the stored record — the lease history does not
//!    grow;
//! 7. the successor registry reopens, and the final durable tally shows
//!    exactly one of everything the chain should hold — no second
//!    durable effect anywhere (no double commit).
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. Citing it as
//! cross-host / network-partition / cross-clock evidence would violate
//! RISK-B-11. Endpoint coverage declared: the `TaskStore` participant
//! endpoint only (the six-type mapping is Phase 1 CS1-C work).

mod common;

use common::{
    FileSignChannel, acquire_incumbent_lease, assignment_state_codes, bootstrap_barrier_signer,
    commit_segment, fixed_hex, heartbeat_counter, hex, lease_history_rows, publish, raw_count,
    spawn_fault_child, spawn_heartbeat, wait_for_file_while_alive, wait_heartbeat_at_least,
};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{BarrierObservationDemand, BarrierObservationSource as _};
use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRecord,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverCompletionRecord,
    AuthorityTakeoverReceiptState, CompleteAuthorityTakeoverRequest, ParticipantRegistryState,
    SqliteTaskAuthority,
};
use nlos_types::{ControlDomainId, IdempotencyKey, KeyId, PrincipalId, TaskId};
use std::path::Path;

const STORE_DIR_ENV: &str = "NLOS_CS0D_DUP_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_DUP_RELEASE";
const BLOCK_ENV: &str = "NLOS_CS0D_DUP_BLOCK";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_DUP_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_DUP_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_DUP_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_DUP_RESP";

const TASK_SEED: u8 = 0xDF;
const CHILD_SEED: u8 = 0xCE;
const CHILD_HOLDER_TAG: u8 = 0xC0;
const PARENT_HOLDER_TAG: u8 = 0xC1;
const SIGNER_SEED: u8 = 0xAE;

/// Logical clock: the incumbent term-1 lease at 100 with TTL 100; the
/// successor works at 300+ (past expiry — the legal takeover window).
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_LEASE_AT_MS: i64 = 300;
const OBSERVED_AT_MS: i64 = 310;
const COMPLETED_AT_MS: i64 = 320;
const REOPENED_AT_MS: i64 = 330;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

/// Harness-only entry: Cell A, the old authority (same shape as the
/// migration category's child).
#[test]
#[ignore = "spawned by duplicate_message_replays_are_idempotent"]
fn child_duplicate_old_cell_helper() {
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
        eprintln!("skipped: cs0d duplicate child env unset (harness entry, parent spawn only)");
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
        0x2E,
        0,
        LEASE_ONE_AT_MS + 10,
        2,
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
    common::serve_observations(
        Path::new(&sign_request),
        Path::new(&sign_response),
        Path::new(&release),
        Path::new(&block),
        Path::new(&heartbeat),
        &ed25519_dalek::SigningKey::from_bytes(&[SIGNER_SEED; 32]),
    );
    heartbeat_thread.join().expect("heartbeat thread");
}

/// The duplicate-message category: channel redelivery, observation-gate
/// replay, completion replay, and lease replay all land on byte-equal
/// records with no second durable effect.
#[test]
#[allow(clippy::too_many_lines)]
fn duplicate_message_replays_are_idempotent() {
    let store_dir = common::TempDir::new("duplicate-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-dup-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-dup-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-dup-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-dup-resp-{stamp}.txt"));
    let block = base.join(format!("nlos-slice-k-cs0d-dup-block-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-dup-release-{stamp}.flag"));
    for path in [
        &out,
        &heartbeat,
        &sign_request,
        &sign_response,
        &block,
        &release,
    ] {
        let _ = std::fs::remove_file(path);
    }

    let mut child = spawn_fault_child(
        "child_duplicate_old_cell_helper",
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

    // Stage 1: registration.
    let registration = wait_for_file_while_alive(&mut child, "duplicate child registration", &out);
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
    let parent_holder = common::holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);
    let store = SqliteTaskAuthority::open(&store_path).expect("successor opens the shared store");
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");

    // Stage 2: term-2 lease and fence through the store's own gates.
    let lease_request = AuthorityLeaseRequest {
        holder_id: parent_holder,
        idempotency_key: IdempotencyKey::from_bytes([0x4F; 16]),
        requested_at_ms: PARENT_LEASE_AT_MS,
        ttl_ms: PARENT_LEASE_TTL_MS,
    };
    let lease = match store
        .acquire_authority_lease(lease_request)
        .expect("term-2 lease")
    {
        AuthorityLeaseDecision::TakenOver(record) => record,
        other => panic!("expected a takeover acquire, got {other:?}"),
    };
    assert_eq!(lease.term, 2);
    let old_assignment = store
        .inspect_authority_assignment(task_id())
        .expect("baseline assignment");
    store
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: old_assignment.participant_registry_binding,
            lease,
            requested_at_ms: PARENT_LEASE_AT_MS + 5,
        })
        .expect("prepare the fence");
    let frozen = store
        .inspect_participant_registry(task_id())
        .expect("frozen registry");
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    let frozen_binding = nlos_task::ParticipantRegistryBinding {
        generation: frozen.generation,
        root: frozen.root,
    };
    let fence_receipt = store
        .inspect_authority_takeover_fence_receipt(task_id(), frozen_binding)
        .expect("fence receipt");
    let exact_fence_set_root = fence_receipt
        .exact_fence_set_root
        .expect("resolvable exact fence-set root");
    let pending = store
        .inspect_authority_takeover_receipt(task_id(), fence_receipt.receipt_id)
        .expect("pending receipt");
    let members = store
        .inspect_authority_takeover_fence_members(task_id(), frozen_binding)
        .expect("manifest");
    assert_ne!(
        members,
        [] as [nlos_task::AuthorityTakeoverFenceMemberRecord; 0],
        "the frozen manifest names its members"
    );

    // Stage 3: channel redelivery — the same demand served twice yields
    // byte-identical observations (the endpoint derivation is
    // deterministic).
    let mut channel = FileSignChannel {
        child: &mut child,
        request_path: sign_request.clone(),
        response_path: sign_response.clone(),
        issuer,
        control_domain_id: domain,
        key_id,
    };
    let member = members[0].participant;
    let demand = BarrierObservationDemand {
        takeover_receipt_id: pending.receipt_id,
        participant: member,
        fence_set_root: exact_fence_set_root,
    };
    let first_delivery = channel.observe_barrier(demand).expect("first delivery");
    let redelivery = channel.observe_barrier(demand).expect("redelivered demand");
    assert_eq!(
        first_delivery, redelivery,
        "a redelivered demand yields the byte-identical observation"
    );

    // Stage 4: gate replay — the second submission of the same bytes
    // returns the byte-equal record and writes nothing new.
    let submit = |observation: &nlos_slice_k::SignedBarrierObservation| {
        store
            .record_authority_takeover_barrier_receipt_signed(
                &identity,
                AuthorityTakeoverBarrierReceiptRequest {
                    takeover_receipt_id: pending.receipt_id,
                    participant: member,
                    remote_receipt_id: observation.remote_receipt_id,
                    barrier_digest: observation.barrier_digest,
                    observed_at_ms: OBSERVED_AT_MS,
                },
                observation.signature,
            )
            .expect("the signed observation gate accepts the material")
    };
    let record_first: AuthorityTakeoverBarrierReceiptRecord = submit(&first_delivery);
    let record_replay: AuthorityTakeoverBarrierReceiptRecord = submit(&redelivery);
    assert_eq!(
        record_first, record_replay,
        "the replayed observation returns the byte-equal stored record"
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_barrier_receipts"),
        1,
        "the duplicate submission writes no second durable row"
    );
    drop(channel);

    // Coverage, then completion + completion replay.
    let coverage = store
        .inspect_authority_takeover_barrier_coverage(pending.receipt_id)
        .expect("coverage");
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    let completion_request = CompleteAuthorityTakeoverRequest {
        takeover_receipt_id: pending.receipt_id,
        lease,
        completed_at_ms: COMPLETED_AT_MS,
    };
    let completion: AuthorityTakeoverCompletionRecord = store
        .complete_authority_takeover(completion_request)
        .expect("complete the takeover");
    assert_eq!(
        completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    let assignments_after_completion = assignment_state_codes(&store_path);
    let completion_replay: AuthorityTakeoverCompletionRecord = store
        .complete_authority_takeover(completion_request)
        .expect("the completion gate replays the completed receipt");
    assert_eq!(
        completion, completion_replay,
        "the replayed completion returns the byte-equal record"
    );
    assert_eq!(
        assignment_state_codes(&store_path),
        assignments_after_completion,
        "the replayed completion grows no assignment rows"
    );

    // Stage 5: lease replay — the identical request replays the record
    // and grows no history row.
    let lease_replay = match store
        .acquire_authority_lease(lease_request)
        .expect("lease replay")
    {
        AuthorityLeaseDecision::Replayed(record) => record,
        other => panic!("expected the replayed lease, got {other:?}"),
    };
    assert_eq!(lease, lease_replay, "the replayed lease is byte-equal");
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "the replayed acquisition writes no second history row"
    );

    // Stage 6: reopen and the final no-second-effect tally.
    store
        .reopen_successor_registry(nlos_task::AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.receipt_id,
            lease,
            reopened_at_ms: REOPENED_AT_MS,
        })
        .expect("reopen the successor registry");
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_receipts"),
        1
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_eq!(lease_history_rows(&store_path), vec![(1, 1), (2, 3)]);
    assert_eq!(assignment_state_codes(&store_path), vec![3, 3, 1]);
    let durable =
        nlos_slice_k::durable_commit_prefix(&store, task_id()).expect("final durable prefix");
    assert_eq!(
        durable.entry_count(),
        2,
        "the baseline prefix is unchanged by every replay — no double commit"
    );
    wait_heartbeat_at_least(&mut child, &heartbeat, heartbeat_counter(&heartbeat) + 2);
    assert!(
        child.try_wait().expect("child alive at tally").is_none(),
        "both Cells must be live when the duplicate evidence is cited"
    );

    std::fs::write(&release, b"release").expect("signal child release");
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
        let _ = std::fs::remove_file(path);
    }
}
