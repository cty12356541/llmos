//! CS0-D H6 category **migration, fault variant**: the authority takeover
//! itself is the migration (C-SHARD scope; Process migration is
//! C-MIGRATE, R6). The CS0-A evidence already proves the fault-free
//! success path; this category injects a **transient mid-chain fault**
//! — a one-shot barrier-delivery loss after the durable lease and fence
//! legs have landed — and asserts the migration still completes to the
//! fault-free terminal state.
//!
//! Shape (both OS processes stay alive throughout; the fault is a
//! channel-level delivery loss, not a crash):
//!
//! 1. the successor's drive acquires term 2 over the expired incumbent
//!    lease, prepares the fence, then loses the first barrier delivery
//!    (`TakeoverDriveError::BarrierSource`, the drive's fail-closed
//!    channel error) — the takeover receipt stays `Pending`;
//! 2. the drive is one-shot per receipt: its re-entry guard refuses the
//!    now-pending assignment (asserted), so the resumption runs through
//!    the store's own idempotent gates — the same gates the drive
//!    sequences — over the healthy channel;
//! 3. the completed terminal state equals the fault-free CS0-A shape:
//!    lease history `(1, acquired) (2, taken-over)`, assignment codes
//!    `Fenced, Fenced, Active`, the receipt `Complete` with its successor
//!    assignment, the registry reopened one generation ahead chained to
//!    the frozen root, one signed observation row per manifest member,
//!    and the active assignment bound to the term-2 lease and the
//!    reopened registry.
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. Citing it as
//! cross-host / network-partition / cross-clock evidence would violate
//! RISK-B-11. Endpoint coverage declared: the `TaskStore` participant
//! endpoint only (the six-type mapping is Phase 1 CS1-C work).

mod common;

use std::fs;
use std::path::Path;

use common::{
    CuttableSignChannel, FileSignChannel, acquire_incumbent_lease, assignment_state_codes,
    bootstrap_barrier_signer, commit_segment, demand_count, fixed_hex, heartbeat_counter, hex,
    lease_history_rows, publish, raw_count, spawn_fault_child, spawn_heartbeat,
    wait_for_file_while_alive, wait_heartbeat_at_least,
};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{CrossCellTakeoverRequest, TakeoverDriveError, drive_cross_cell_takeover};
use nlos_task::{
    AuthorityLeaseDecision, AuthorityLeaseRequest, AuthorityTakeoverReceiptState,
    ParticipantRegistryState, SqliteTaskAuthority,
};
use nlos_types::{ControlDomainId, IdempotencyKey, KeyId, PrincipalId, TaskId};

const STORE_DIR_ENV: &str = "NLOS_CS0D_MIG_STORE_DIR";
const RELEASE_ENV: &str = "NLOS_CS0D_MIG_RELEASE";
const BLOCK_ENV: &str = "NLOS_CS0D_MIG_BLOCK";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_MIG_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_MIG_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_MIG_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_MIG_RESP";

const TASK_SEED: u8 = 0xDD;
const CHILD_SEED: u8 = 0xCD;
const CHILD_HOLDER_TAG: u8 = 0xBE;
const PARENT_HOLDER_TAG: u8 = 0xBF;
const SIGNER_SEED: u8 = 0xAD;

/// Logical clock: the incumbent term-1 lease at 100 with TTL 100; the
/// successor drives at 300+ (past expiry — the legal takeover window).
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_CLOCK_BASE_MS: i64 = 295;
const PARENT_CLOCK_STEP_MS: i64 = 10;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

/// Harness-only entry: Cell A, the old authority. Acquires the term-1
/// lease, commits its baseline prefix, bootstraps the shared barrier
/// signer, then serves signed observations (block-flag aware) until
/// released, heartbeating throughout.
#[test]
#[ignore = "spawned by migration_with_mid_chain_fault_reaches_the_fault_free_terminal_state"]
fn child_migration_old_cell_helper() {
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
        eprintln!("skipped: cs0d migration child env unset (harness entry, parent spawn only)");
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
        0x2D,
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

/// The migration fault variant: a transient mid-chain barrier-delivery
/// loss (both Cells alive) fails the drive closed after the durable lease
/// and fence legs, and the migration completes to the fault-free terminal
/// state through the store's own resumption gates.
#[test]
#[allow(clippy::too_many_lines)]
fn migration_with_mid_chain_fault_reaches_the_fault_free_terminal_state() {
    let store_dir = common::TempDir::new("migration-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-mig-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-mig-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-mig-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-mig-resp-{stamp}.txt"));
    let block = base.join(format!("nlos-slice-k-cs0d-mig-block-{stamp}.flag"));
    let release = base.join(format!("nlos-slice-k-cs0d-mig-release-{stamp}.flag"));
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
        "child_migration_old_cell_helper",
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
    let registration = wait_for_file_while_alive(&mut child, "migration child registration", &out);
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
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");
    let hb_baseline = heartbeat_counter(&heartbeat);
    assert!(hb_baseline > 0);

    // Stage 2: the drive with a one-shot mid-chain fault armed at the
    // barrier leg. The lease and fence legs are durable; the first
    // barrier delivery is lost; the drive fails closed with its channel
    // error and the receipt stays Pending.
    let lease_key = IdempotencyKey::from_bytes([0x4D; 16]);
    let attempt_one_lease_at_ms = PARENT_CLOCK_BASE_MS + PARENT_CLOCK_STEP_MS;
    {
        let mut source = CuttableSignChannel::new(FileSignChannel {
            child: &mut child,
            request_path: sign_request.clone(),
            response_path: sign_response.clone(),
            issuer,
            control_domain_id: domain,
            key_id,
        });
        source.fail_next_barrier();
        let mut tick = PARENT_CLOCK_BASE_MS;
        let mut clock = || {
            tick += PARENT_CLOCK_STEP_MS;
            tick
        };
        let fault = drive_cross_cell_takeover(CrossCellTakeoverRequest {
            store: &successor_store,
            identity: &identity,
            barrier_source: &mut source,
            task_id: task_id(),
            successor_holder_id: parent_holder,
            lease_idempotency_key: lease_key,
            lease_ttl_ms: PARENT_LEASE_TTL_MS,
            now_ms: &mut clock,
        })
        .expect_err("the armed mid-chain fault must fail the drive closed");
        assert!(
            matches!(&fault, TakeoverDriveError::BarrierSource(reason)
                if reason.contains("mid-chain fault")),
            "expected the one-shot mid-chain fault, got {fault}"
        );
    }
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "the durable lease leg landed before the fault"
    );
    assert_eq!(assignment_state_codes(&store_path), vec![2]);
    let frozen = successor_store
        .inspect_participant_registry(task_id())
        .expect("registry after the fault");
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    let frozen_binding = nlos_task::ParticipantRegistryBinding {
        generation: frozen.generation,
        root: frozen.root,
    };
    let fence_receipt = successor_store
        .inspect_authority_takeover_fence_receipt(task_id(), frozen_binding)
        .expect("fence receipt after the fault");
    let pending = successor_store
        .inspect_authority_takeover_receipt(task_id(), fence_receipt.receipt_id)
        .expect("pending receipt after the fault");
    assert_eq!(
        pending.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(pending.new_assignment_id, None);
    wait_heartbeat_at_least(&mut child, &heartbeat, heartbeat_counter(&heartbeat) + 2);
    assert_eq!(
        demand_count(&heartbeat),
        0,
        "the faulted delivery never reached the old Cell"
    );
    assert!(
        child.try_wait().expect("child alive at fault").is_none(),
        "both Cells must be alive through the mid-chain fault"
    );

    // Stage 3: the drive's one-shot guard refuses the pending receipt;
    // resumption is the store's own gates.
    {
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
    let successor_lease = match successor_store
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: parent_holder,
            idempotency_key: lease_key,
            requested_at_ms: attempt_one_lease_at_ms,
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
    let mut finish_tick = PARENT_CLOCK_BASE_MS + 90;
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
    .expect("the migration completes through the resumption gates");
    drop(source);

    // Stage 4: the terminal state equals the fault-free CS0-A shape.
    assert_eq!(
        finished.completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(
        finished.completion.old_assignment_id, pending.old_assignment_id,
        "the CAS chain's subject is unchanged by the fault"
    );
    assert_eq!(
        lease_history_rows(&store_path),
        vec![(1, 1), (2, 3)],
        "fault-free lease history shape"
    );
    assert_eq!(
        assignment_state_codes(&store_path),
        vec![3, 3, 1],
        "fault-free assignment shape: old and completion fenced, reopen active"
    );
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_receipts"),
        1
    );
    let observation_rows = i64::try_from(finished.observations.len()).expect("rows fit i64");
    assert_eq!(
        raw_count(&store_path, "task_authority_takeover_barrier_receipts"),
        observation_rows,
        "one durable observation row per manifest member — no phantom replays"
    );
    assert_eq!(finished.registry.state, ParticipantRegistryState::Open);
    assert_eq!(
        finished.registry.generation,
        frozen_binding.generation + 1,
        "the successor generation is one ahead of the frozen one"
    );
    assert_ne!(finished.registry.root, frozen_binding.root);
    assert_eq!(
        finished.registry.prior_root, frozen_binding.root,
        "the reopened generation chains to the frozen root"
    );
    assert_eq!(
        finished.active_assignment.authority_lease_binding,
        successor_lease.binding()
    );
    assert_eq!(
        finished.active_assignment.participant_registry_binding,
        finished.reopen.successor_registry_binding
    );
    assert!(
        demand_count(&heartbeat) >= finished.observations.len(),
        "the observations were served over the healthy channel"
    );
    assert!(
        child
            .try_wait()
            .expect("child alive at terminal state")
            .is_none(),
        "both Cells must be live when the terminal-state evidence is cited"
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
