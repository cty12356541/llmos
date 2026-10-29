//! CS0-D H6 category **restart**: the old-authority Cell process crashes
//! mid-chain (kill-9 — the crash-legal injection, per the R2.2 item 3
//! methodological constraint kill belongs to this category, not to
//! partition), a restarted process of the same Cell identity continues
//! the chain **from the durable prefix**, and the successor's takeover
//! and convergence over the restarted trail satisfy the four ADR-0013
//! decision 1 contract invariants (ADR-0019 R1.2.4: carried over
//! verbatim) with byte-identical replay.
//!
//! Chain shape (ADR-0018: one OS process = one Cell; the restart is a NEW
//! OS process with the SAME `CellIdentity` — the identity is
//! domain-derived, not pid-derived):
//!
//! 1. the first process commits segment one (two durable entries),
//!    records its structured checkpoint through its federated
//!    [`nlos_slice_k::CellHost`] wiring, and is then killed mid-chain —
//!    between its checkpoint and its remaining workload;
//! 2. the restarted process opens the same durable store, re-derives its
//!    commit prefix through the same public [`durable_commit_prefix`]
//!    the verifier uses (the from-durable-prefix continuation), commits
//!    segment two, records its own structured checkpoint at its own
//!    boot's fence axes, commits segment three (the post-restart
//!    crash-window gap), and serves the takeover's barrier observations;
//! 3. the successor drives the full takeover (term 2 over the expired
//!    incumbent lease) and converges: the verified anchor is the
//!    restarted process's checkpoint (its trail axes name the restarted
//!    pid), the adopted gap is exactly segment three, all four contract
//!    invariants hold, and an identical rerun produces a byte-identical
//!    convergence record.
//!
//! **Crash semantics disclaimer** (the `nlos-task` fault-matrix
//! convention): kill-9 simulates a *process* crash; the OS page cache
//! survives it, so this is not a machine power-loss row.
//!
//! **Evidence scope (honesty bound):** single-host, dual-process evidence
//! — shared filesystem, shared kernel, shared clock domain. Citing it as
//! cross-host / network-partition / cross-clock evidence would violate
//! RISK-B-11. Endpoint coverage declared: the `TaskStore` participant
//! endpoint only (the six-type mapping is Phase 1 CS1-C work).

mod common;

use std::path::Path;

use common::{
    CuttableSignChannel, FileSignChannel, acquire_incumbent_lease, assert_invariants_all_hold,
    commit_segment, demand_count, fixed_hex, heartbeat_counter, hex, holder_from_pid,
    kill_and_reap, publish, remove_file_if_exists_retried, remove_file_retried, spawn_fault_child,
    spawn_heartbeat, wait_for_file_while_alive, wait_heartbeat_at_least, write_retried,
};
use ed25519_dalek::SigningKey;
use nlos_cell::{CellDirectory, CellIdentity, CheckpointDigest, CheckpointFact};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{
    CrossCellTakeoverRequest, PrefixVisibility, ReconciliationRequest, TrailCover,
    converge_commit_prefix_gap, drive_cross_cell_takeover, durable_commit_prefix,
};
use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId, SchedulerDomainId, TaskId,
};

const STORE_DIR_ENV: &str = "NLOS_CS0D_RESTART_STORE_DIR";
const ROLE_ENV: &str = "NLOS_CS0D_RESTART_ROLE";
const RELEASE_ENV: &str = "NLOS_CS0D_RESTART_RELEASE";
const HEARTBEAT_ENV: &str = "NLOS_CS0D_RESTART_HEARTBEAT";
const OUT_ENV: &str = "NLOS_CS0D_RESTART_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0D_RESTART_REQ";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0D_RESTART_RESP";

const TASK_SEED: u8 = 0xDB;
const FIRST_SEED: u8 = 0xCB;
const CHILD_HOLDER_TAG: u8 = 0xBC;
const PARENT_HOLDER_TAG: u8 = 0xBD;
const SIGNER_SEED: u8 = 0xAB;

/// Logical clock: the term-1 lease runs at 100 with TTL 100 (expires at
/// 200); the first process's segment and the restarted process's two
/// segments all commit inside that window; the successor drives at 300+.
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const SEGMENT_ONE_AT_MS: i64 = 110;
const SEGMENT_TWO_AT_MS: i64 = 130;
const SEGMENT_THREE_AT_MS: i64 = 150;
const PARENT_LEASE_TTL_MS: i64 = 100_000;
const PARENT_CLOCK_BASE_MS: i64 = 295;
const PARENT_CLOCK_STEP_MS: i64 = 10;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

fn cell_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xEB; 16])
}

fn cell_identity() -> CellIdentity {
    CellIdentity::from_domain(cell_domain())
}

/// Per-process-generation checkpoint fence axes. The federated
/// registration face models single-occupancy per Cell identity (a second
/// `open_federated` of the same identity refuses on the heartbeat
/// monotonicity gate), so each process generation of the restarted Cell
/// records its structured checkpoint through the shared
/// [`CellDirectory`] face directly, at its own boot generation — the
/// digest grammar is public ([`CheckpointDigest::from_prefix`]) and the
/// convergence core verifies at each record's own axes.
const FIRST_BOOT: u64 = 7;
const RESTART_BOOT: u64 = 8;
const CHECKPOINT_EPOCH: u64 = 11;
const CHECKPOINT_TOKEN: u64 = 13;

/// Records one structured checkpoint pinning the authority's current
/// durable prefix at the given boot generation's fence axes, through the
/// shared federation directory.
fn record_generation_checkpoint(
    authority: &nlos_task::SqliteTaskAuthority,
    store_dir: &str,
    boot: u64,
) -> CheckpointDigest {
    let generation = Generation::new(std::num::NonZeroU64::new(boot).expect("nonzero boot"));
    let epoch = nlos_cell::CellEpoch::from_u64(CHECKPOINT_EPOCH).expect("epoch");
    let token = nlos_cell::CellFencingToken::from_u64(CHECKPOINT_TOKEN).expect("token");
    let durable = durable_commit_prefix(authority, task_id()).expect("derive durable prefix");
    let digest = CheckpointDigest::from_prefix(generation, epoch, token, durable.prefix());
    let fact = CheckpointFact::new(generation, epoch, token, &digest.to_text())
        .expect("structured checkpoint fact");
    CellDirectory::open(Path::new(store_dir))
        .expect("open the shared federation directory")
        .record_checkpoint(cell_identity(), &fact)
        .expect("record the structured checkpoint");
    digest
}

/// Harness-only entry: the old-authority Cell, in its two process
/// generations. `first` establishes the term, commits segment one,
/// checkpoints, registers the shared barrier signer, then idles until
/// killed mid-chain. `restart` re-opens the same durable store,
/// continues from the durable prefix (segment two, its own checkpoint,
/// segment three), then serves barrier observations until released.
#[test]
#[ignore = "spawned by restart_continues_from_durable_prefix_and_converges"]
#[allow(clippy::too_many_lines)]
fn child_restart_old_cell_helper() {
    let (
        Ok(store_dir),
        Ok(role),
        Ok(release),
        Ok(heartbeat),
        Ok(out),
        Ok(sign_request),
        Ok(sign_response),
    ) = (
        std::env::var(STORE_DIR_ENV),
        std::env::var(ROLE_ENV),
        std::env::var(RELEASE_ENV),
        std::env::var(HEARTBEAT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
    )
    else {
        eprintln!("skipped: cs0d restart child env unset (harness entry, parent spawn only)");
        return;
    };
    let store_path = Path::new(&store_dir).join("task-authority.sqlite3");
    let authority = nlos_task::SqliteTaskAuthority::open(&store_path)
        .expect("the old Cell opens its task store");
    let heartbeat_thread = spawn_heartbeat(Path::new(&heartbeat), Path::new(&release));

    if role == "first" {
        // --- First process generation: term 1, segment one, checkpoint,
        // shared signer bootstrap. The remainder of its chain (segments
        // two and three, barrier service) dies with the kill below.
        let lease_one = acquire_incumbent_lease(
            &authority,
            holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
            FIRST_SEED,
            LEASE_ONE_AT_MS,
            LEASE_ONE_TTL_MS,
        );
        assert_eq!(lease_one.term, 1);
        common::register_task_and_attempt(&authority, task_id(), 0x1A, LEASE_ONE_AT_MS + 1);
        commit_segment(
            &authority,
            &lease_one,
            task_id(),
            0x21,
            0,
            SEGMENT_ONE_AT_MS,
            2,
        );
        let identity = cell_identity();
        let digest = record_generation_checkpoint(&authority, &store_dir, FIRST_BOOT);
        let identity_authority = IdentityAuthority::open(Path::new(&store_dir).join("identity"))
            .expect("first process opens the identity root");
        let binding = common::bootstrap_barrier_signer(&identity_authority, SIGNER_SEED);
        publish(
            Path::new(&out),
            &format!(
                "registered\n{}\n{}\n{}\n{}\n{}\n{}\n",
                std::process::id(),
                hex(identity.as_bytes()),
                digest.to_text(),
                hex(binding.principal_id.as_bytes()),
                hex(binding.control_domain_id.as_bytes()),
                hex(binding.key_id.as_bytes()),
            ),
        );
        // Mid-chain idle: the parent kills this process here (kill-9).
        let deadline = std::time::Instant::now() + common::CHILD_WAIT;
        while !Path::new(&release).exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "first process timed out waiting for the mid-chain kill"
            );
            std::thread::sleep(common::POLL);
        }
        heartbeat_thread.join().expect("heartbeat thread");
        return;
    }

    // --- Restarted process generation: same Cell identity (same domain),
    // new OS process. Everything it knows about the chain it re-derives
    // from the durable store — the from-durable-prefix continuation.
    assert_eq!(role, "restart", "unknown role {role}");
    let lease_one = authority
        .inspect_authority_lease()
        .expect("re-read the durable incumbent lease");
    assert_eq!(lease_one.term, 1);
    let resumed = durable_commit_prefix(&authority, task_id())
        .expect("re-derive the durable prefix after the crash");
    assert_eq!(
        resumed.entry_count(),
        2,
        "the restarted process resumes from the crashed process's durable prefix"
    );
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x22,
        16,
        SEGMENT_TWO_AT_MS,
        2,
    );
    let identity = cell_identity();
    let digest = record_generation_checkpoint(&authority, &store_dir, RESTART_BOOT);
    commit_segment(
        &authority,
        &lease_one,
        task_id(),
        0x23,
        32,
        SEGMENT_THREE_AT_MS,
        2,
    );
    let durable = durable_commit_prefix(&authority, task_id()).expect("prefix after segment three");
    assert_eq!(durable.entry_count(), 6);
    publish(
        Path::new(&out),
        &format!(
            "registered\n{}\n{}\n{}\nrestarted\n",
            std::process::id(),
            hex(identity.as_bytes()),
            digest.to_text(),
        ),
    );
    common::serve_observations(
        Path::new(&sign_request),
        Path::new(&sign_response),
        Path::new(&release),
        Path::new(&store_dir).join("block-unused.flag").as_path(),
        Path::new(&heartbeat),
        &SigningKey::from_bytes(&[SIGNER_SEED; 32]),
    );
    heartbeat_thread.join().expect("heartbeat thread");
}

/// The restart category: a mid-chain kill-9 of the old Cell, a restarted
/// process continuing from the durable prefix, and a successor takeover
/// whose convergence satisfies the four contract invariants with
/// byte-identical replay.
#[test]
#[allow(clippy::too_many_lines)]
fn restart_continues_from_durable_prefix_and_converges() {
    let store_dir = common::TempDir::new("restart-store");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let stamp = std::process::id();
    let base = std::env::temp_dir();
    let out = base.join(format!("nlos-slice-k-cs0d-restart-out-{stamp}.txt"));
    let heartbeat = base.join(format!("nlos-slice-k-cs0d-restart-hb-{stamp}.txt"));
    let sign_request = base.join(format!("nlos-slice-k-cs0d-restart-req-{stamp}.txt"));
    let sign_response = base.join(format!("nlos-slice-k-cs0d-restart-resp-{stamp}.txt"));
    let release = base.join(format!("nlos-slice-k-cs0d-restart-release-{stamp}.flag"));
    for path in [&out, &heartbeat, &sign_request, &sign_response, &release] {
        let _ = remove_file_if_exists_retried(path);
    }
    let envs = |role: &str| {
        [
            (STORE_DIR_ENV, store_dir.path().as_os_str().to_os_string()),
            (ROLE_ENV, role.to_string().into()),
            (RELEASE_ENV, release.as_os_str().to_os_string()),
            (HEARTBEAT_ENV, heartbeat.as_os_str().to_os_string()),
            (OUT_ENV, out.as_os_str().to_os_string()),
            (SIGN_REQUEST_ENV, sign_request.as_os_str().to_os_string()),
            (SIGN_RESPONSE_ENV, sign_response.as_os_str().to_os_string()),
        ]
    };
    let read_registration = |text: &str| {
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("registered"));
        let pid: u32 = lines
            .next()
            .expect("child pid line")
            .parse()
            .expect("child pid u32");
        let identity = nlos_cell::CellIdentity::from_domain(SchedulerDomainId::from_bytes(
            fixed_hex(lines.next().expect("identity line")),
        ));
        let digest = lines.next().expect("checkpoint digest line").to_string();
        (pid, identity, digest)
    };

    // --- First process generation: register, then kill mid-chain.
    let mut first = spawn_fault_child("child_restart_old_cell_helper", &envs("first"));
    let first_registration =
        wait_for_file_while_alive(&mut first, "first process registration", &out);
    let (first_pid, old_cell, _first_digest) = read_registration(&first_registration);
    // The first registration carries the signer triple after its digest
    // line (indices 4..=6).
    let signer_lines: Vec<&str> = first_registration.lines().skip(4).take(3).collect();
    let issuer = PrincipalId::from_bytes(fixed_hex(signer_lines[0]));
    let domain = ControlDomainId::from_bytes(fixed_hex(signer_lines[1]));
    let key_id = KeyId::from_bytes(fixed_hex(signer_lines[2]));
    wait_heartbeat_at_least(&mut first, &heartbeat, 1);
    let hb_before_kill = heartbeat_counter(&heartbeat);
    assert!(hb_before_kill > 0);
    assert_ne!(first_pid, std::process::id());
    assert!(
        first.try_wait().expect("first alive before kill").is_none(),
        "the first process must be alive at its registration evidence"
    );
    kill_and_reap(&mut first); // the mid-chain crash (kill-9)

    // --- Restarted process generation: same Cell identity, new process.
    // Its registration goes to the same receipt path, so the first
    // generation's receipt is cleared first (wait_for_file must not read
    // the stale one).
    remove_file_retried(&out).expect("clear the first generation's receipt");
    let mut child = spawn_fault_child("child_restart_old_cell_helper", &envs("restart"));
    let restart_registration =
        wait_for_file_while_alive(&mut child, "restarted process registration", &out);
    wait_heartbeat_at_least(&mut child, &heartbeat, hb_before_kill + 1);
    let (restart_pid, restart_cell, restart_digest) = read_registration(&restart_registration);
    assert_eq!(
        restart_cell, old_cell,
        "the restart keeps the same Cell identity (domain-derived)"
    );
    assert_ne!(restart_pid, first_pid, "the restart is a new OS process");
    assert!(
        restart_registration.lines().any(|line| line == "restarted"),
        "the registration must carry the restart marker"
    );
    let successor_store = nlos_task::SqliteTaskAuthority::open(&store_path)
        .expect("successor opens the shared task store");
    let durable = durable_commit_prefix(&successor_store, task_id())
        .expect("durable prefix after the restart's continuation");
    assert_eq!(durable.entry_count(), 6);

    // --- The successor drives the full takeover over the expired
    // incumbent lease; the barrier observations come from the restarted
    // process (same signer key the first process bootstrapped).
    let parent_holder = holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);
    let identity = IdentityAuthority::open(store_dir.path().join("identity"))
        .expect("successor opens identity root");
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
    let takeover = drive_cross_cell_takeover(CrossCellTakeoverRequest {
        store: &successor_store,
        identity: &identity,
        barrier_source: &mut source,
        task_id: task_id(),
        successor_holder_id: parent_holder,
        lease_idempotency_key: IdempotencyKey::from_bytes([0x4B; 16]),
        lease_ttl_ms: PARENT_LEASE_TTL_MS,
        now_ms: &mut clock,
    })
    .expect("the successor drives the takeover over the restarted Cell");
    drop(source);
    assert_eq!(takeover.successor_lease.term, 2);
    assert_eq!(
        demand_count(&heartbeat),
        takeover.observations.len(),
        "the restarted process served every barrier observation"
    );
    assert!(
        child.try_wait().expect("child alive at takeover").is_none(),
        "the restarted Cell must be live when the takeover completes"
    );

    // --- Convergence: the restarted process's checkpoint is the anchor
    // (its trail axes name the restarted pid), segment three is exactly
    // the adopted gap, all four invariants hold.
    let directory = nlos_cell::CellDirectory::open(store_dir.path())
        .expect("successor opens the shared federation root");
    let outcome = converge_commit_prefix_gap(ReconciliationRequest {
        store: &successor_store,
        directory: &directory,
        authority_cell: old_cell,
        task_id: task_id(),
        term_lease: takeover.successor_lease,
    })
    .expect("the successor converges the restart's crash window");
    assert_eq!(outcome.covered_entries, 4);
    assert_eq!(outcome.visibility, PrefixVisibility::Partial);
    let anchor = outcome.anchor.as_ref().expect("verified anchor");
    assert_eq!(anchor.digest.to_text(), restart_digest);
    assert_eq!(
        anchor.os_process_id, restart_pid,
        "the anchor names the restarted process, not the crashed one"
    );
    assert_ne!(anchor.os_process_id, first_pid);
    assert_eq!(
        outcome
            .adopted
            .iter()
            .map(|entry| entry.effect_history_seq)
            .collect::<Vec<_>>(),
        vec![5, 6],
        "the gap adopts exactly the post-restart-checkpoint segment"
    );
    assert_eq!(outcome.final_state.entry_count, 6);
    assert_eq!(outcome.final_state.head_commit_seq, 3);
    assert_invariants_all_hold(&outcome.invariants);
    assert_eq!(
        outcome.trail.len(),
        2,
        "both process generations' checkpoints are on the trail"
    );
    assert!(matches!(
        outcome.trail[0].cover,
        TrailCover::Anchored { covered_entries: 2 }
    ));
    assert!(matches!(
        outcome.trail[1].cover,
        TrailCover::Anchored { covered_entries: 4 }
    ));

    // --- Byte-identical replay of the convergence.
    let replay = converge_commit_prefix_gap(ReconciliationRequest {
        store: &successor_store,
        directory: &directory,
        authority_cell: old_cell,
        task_id: task_id(),
        term_lease: takeover.successor_lease,
    })
    .expect("replay convergence succeeds");
    assert_eq!(outcome, replay, "replay converges to the identical record");
    assert_eq!(
        outcome.invariants.replay_digest, replay.invariants.replay_digest,
        "replay digests compare byte-exactly"
    );
    wait_heartbeat_at_least(&mut child, &heartbeat, heartbeat_counter(&heartbeat) + 2);
    assert!(
        child.try_wait().expect("child alive at replay").is_none(),
        "both Cells must be live when the restart evidence is cited"
    );

    write_retried(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(
        status.success(),
        "restarted Cell process failed: {status:?}"
    );
    for path in [&out, &heartbeat, &sign_request, &sign_response, &release] {
        let _ = remove_file_if_exists_retried(path);
    }
}
