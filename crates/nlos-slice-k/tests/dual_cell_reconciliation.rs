//! CS0-C: the evidence of the reconciliation convergence core — ADR-0019
//! R1.2.4's bounded convergence of the commit-prefix gap between the
//! takeover-completion point and the old authority's last checkpoint,
//! with the ADR-0013 decision 1 contract invariants (crash-window
//! convergence, no phantom rows, no double commit, byte-identical replay)
//! carried over verbatim, not re-invented here.
//!
//! Two evidence layers, declared separately:
//!
//! 1. **Single-process core scenarios** — the convergence core driven
//!    against one local store and one synthetic Cell trail through the
//!    public faces only: gap coverage from a verified anchor, byte-exact
//!    idempotent replay, anchor advance adopting nothing (the no-double-
//!    commit behavioral path), tampered / corrupt structured digests
//!    failing closed (the phantom-claim refusal), free-text digests
//!    covering nothing (`PARTIAL` / `UNCERTAIN`, R1.2.3), and the
//!    stale-term refusal.
//! 2. **Dual-Cell evidence** — two independent OS processes (ADR-0018:
//!    one process = one Cell; shared kernel, clock domain, and
//!    filesystem): the child is Cell A, the old authority — it holds the
//!    term-1 lease, commits two prefix segments with declared effects,
//!    registers one structured checkpoint (covering the first segment)
//!    through its federated [`CellHost`] wiring, and stays alive
//!    throughout; the parent is Cell B, the successor — after Cell A's
//!    lease expires it drives the full CS0-A takeover chain over the same
//!    store, then converges the uncovered gap
//!    ([`converge_commit_prefix_gap`]), replays the convergence
//!    byte-exactly, converges again from a trail with no verified anchor
//!    (a different convergence entry, the same unique final state), and
//!    finally receipts that the still-live old Cell's own convergence
//!    attempt under its stale term-1 lease is refused.
//!
//! **Evidence scope (honesty bound):** this is *single-host* evidence —
//! the dual-Cell scenario is dual-*process* on one shared filesystem,
//! shared kernel, shared clock domain. It is exactly what ADR-0019
//! decision 1 defers cross-machine semantics behind; citing it as
//! cross-host / network-partition / cross-clock evidence would violate
//! RISK-B-11. This is the minimal convergence core, not a full executor,
//! not `C-MIGRATE`. Endpoint coverage declared: the `TaskStore` authority
//! only (the same declared coverage as the CS0-A drive); the six-type
//! endpoint mapping is Phase 1 (CS1-C) work and no other coverage is
//! claimed. The old Cell's stale-term refusal proves the entry guard of
//! this core; it is not a partition or dual-primary *injection* (no
//! communication is blocked and both leases are not live at once) — the
//! H6 matrix owns those categories (CS0-D).
//!
//! Harness follows the `nlos-cell` dual-process convention: `#[ignore]`
//! child helpers spawned from this very test binary via env + temp-file
//! signaling, no product IPC. One parent `#[test]` owns the store-scoped
//! drive exactly once.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use nlos_cell::{
    CellDirectory, CellIdentity, CheckpointDigest, CheckpointFact, FailureDetectorConfig,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_slice_k::{
    BarrierObservationDemand, BarrierObservationSource, ConvergenceInvariants,
    CrossCellTakeoverRequest, PrefixVisibility, ReconciliationError, ReconciliationRequest,
    SignedBarrierObservation, TakeoverDriveError, TrailCover, converge_commit_prefix_gap,
    drive_cross_cell_takeover, durable_commit_prefix,
};
use nlos_task::{
    AttemptSpec, AuthorityLeaseDecision, AuthorityLeaseDispatchRequest,
    AuthorityLeaseEffectPermitRequest, AuthorityLeaseFinalizeRequest, AuthorityLeaseOutcomeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRequest, DispatchRequest, EffectPermitDecision,
    FinalizeRequest, FinalizeRequestV3, LogicalEffectDescriptor, Outcome, OutcomeRequest,
    PermitDecision, PermitRequest, PlannedEffect, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
};
use nlos_types::{
    CancellationScopeId, ControlDomainId, DeviceId, Generation, IdempotencyKey, KeyId, PrincipalId,
    ProcessId, ReceiptId, SchedulerDomainId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use sha2::Digest;

/// The Task every durable fact of this file is about.
const TASK_SEED: u8 = 0xD3;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

/// Synthetic fence axes the single-process checkpoint authors use — the
/// executor verifies at the fact's own axes, so any consistent axes pin a
/// verifiable claim.
const CORE_BOOT: u64 = 7;
const CORE_EPOCH: u64 = 11;
const CORE_TOKEN: u64 = 13;

fn core_generation() -> Generation {
    Generation::new(NonZeroU64::new(CORE_BOOT).expect("nonzero boot"))
}

fn core_epoch() -> nlos_cell::CellEpoch {
    nlos_cell::CellEpoch::from_u64(CORE_EPOCH).expect("epoch")
}

fn core_token() -> nlos_cell::CellFencingToken {
    nlos_cell::CellFencingToken::from_u64(CORE_TOKEN).expect("token")
}

fn synthetic_cell(seed: u8) -> CellIdentity {
    CellIdentity::from_domain(SchedulerDomainId::from_bytes([seed; 16]))
}

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("nlos-slice-k-cs0c-{prefix}-{}", std::process::id()));
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
            Err(error) => panic!("remove cs0c temp root: {error}"),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex nibble");
    }
    out
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        other => panic!("hex nibble byte {}", char::from(other)),
    }
}

fn fixed_hex<const N: usize>(line: &str) -> [u8; N] {
    let bytes = line.as_bytes();
    assert_eq!(bytes.len(), N * 2, "hex line must be {N} bytes wide");
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[2 * index]) << 4) | hex_nibble(bytes[2 * index + 1]);
    }
    out
}

fn holder_from_pid(pid: u32, tag: u8) -> ProcessId {
    let mut bytes = [0_u8; 16];
    bytes[..4].copy_from_slice(&pid.to_be_bytes());
    bytes[15] = tag;
    ProcessId::from_bytes(bytes)
}

/// One local authority fixture: a fresh store, a term-1 lease held by this
/// process, the Task registered, and a federation directory for trails.
struct CoreFixture {
    _dir: TempDir,
    authority: SqliteTaskAuthority,
    directory: CellDirectory,
    lease: nlos_task::AuthorityLeaseRecord,
}

fn core_fixture(name: &str, holder_tag: u8) -> CoreFixture {
    let dir = TempDir::new(name);
    let authority = SqliteTaskAuthority::open(dir.path().join("task-authority.sqlite3"))
        .expect("open core fixture store");
    let lease = match authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: holder_from_pid(std::process::id(), holder_tag),
            idempotency_key: IdempotencyKey::from_bytes([0x11; 16]),
            requested_at_ms: 100,
            ttl_ms: 100_000,
        })
        .expect("acquire the fixture lease")
    {
        AuthorityLeaseDecision::Acquired(record) => record,
        other => panic!("expected a fresh acquire, got {other:?}"),
    };
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 101,
        })
        .expect("register the fixture task");
    let directory =
        CellDirectory::open(dir.path().join("federation")).expect("open federation root");
    CoreFixture {
        _dir: dir,
        authority,
        directory,
        lease,
    }
}

fn planned_effect(slot_base: u64, slot: u64, seed: u8) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [seed; 32],
            stable_action_slot: slot_base + slot,
            target_authority_object_id: [0x55; 32],
            effect_class: 7,
            idempotency_scope: 3,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

/// Commits one prefix segment under the held lease: one attempt, one
/// lease-bound permit with `effects` declared (all optional) effects,
/// every slot closed with an effect (each closure appends one durable
/// history entry), then the finalize that advances the `TaskHead`.
fn commit_segment(
    authority: &SqliteTaskAuthority,
    lease: &nlos_task::AuthorityLeaseRecord,
    seed: u8,
    slot_base: u64,
    at_ms: i64,
    effects: u64,
) {
    let head = authority
        .inspect_task(task_id())
        .expect("task head before segment");
    let attempt = AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([seed; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(1); 16]),
            snapshot_digest: [seed.wrapping_add(2); 32],
            expected_head_commit_seq: head.head_commit_seq,
            effect_history_root: head.head_effect_history_root,
            retry_fence_epoch: head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(3); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        registered_at_ms: at_ms,
    };
    authority
        .register_attempt(attempt)
        .expect("register segment attempt");
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [seed; 32],
                planned_effects: (0..effects)
                    .map(|slot| planned_effect(slot_base, slot, seed))
                    .collect(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
                valid_until_ms: 1_000_000,
                requested_at_ms: at_ms + 1,
            },
            lease: *lease,
        })
        .expect("issue the segment's lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    for slot in 0..effects {
        close_effect_slot(authority, lease, &attempt, &permit, seed, slot, at_ms);
    }
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    new_effect_history_root: [0; 32],
                    new_retry_fence_epoch: 0,
                    finalized_at_ms: at_ms + 5,
                },
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
            },
            lease: *lease,
        })
        .expect("finalize the segment commit");
}

/// Closes one declared effect slot under the held lease: effect permit →
/// dispatch token → recorded outcome (the closure that appends the
/// durable history entry).
fn close_effect_slot(
    authority: &SqliteTaskAuthority,
    lease: &nlos_task::AuthorityLeaseRecord,
    attempt: &AttemptSpec,
    permit: &nlos_task::PermitRecord,
    seed: u8,
    slot: u64,
    at_ms: i64,
) {
    // Effect-permit idempotency keys namespace per task (cross attempt),
    // so the key must differ for every (segment, slot) pair — mixed
    // bytes, not one repeated byte.
    let slot_tag = u8::try_from(slot).expect("test slot fits u8");
    let mut effect_key = [0xEC_u8; 16];
    effect_key[1] = seed;
    effect_key[2] = slot_tag;
    let issued = match authority
        .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
            permit: nlos_task::EffectPermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: slot,
                idempotency_key: IdempotencyKey::from_bytes(effect_key),
                valid_until_ms: 1_000_000,
                requested_at_ms: at_ms + 2,
            },
            lease: *lease,
        })
        .expect("issue the effect permit")
    {
        EffectPermitDecision::Issued(record) => record,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    };
    authority
        .consume_dispatch_token_with_authority_lease(AuthorityLeaseDispatchRequest {
            dispatch: DispatchRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_permit_id: issued.effect_permit_id,
                dispatch_token: issued.one_shot_dispatch_token,
                dispatched_at_ms: at_ms + 3,
            },
            lease: *lease,
        })
        .expect("consume the dispatch token");
    authority
        .record_effect_outcome_with_authority_lease(AuthorityLeaseOutcomeRequest {
            outcome: OutcomeRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: slot,
                outcome: Outcome::Closed {
                    authoritative_closure_digest: [seed.wrapping_add(slot_tag); 32],
                },
                recorded_at_ms: at_ms + 4,
            },
            lease: *lease,
        })
        .expect("close the effect slot");
}

/// Registers one structured checkpoint for `cell` whose digest pins the
/// authority's current durable prefix (derived through the same public
/// [`durable_commit_prefix`] the executor verifies with).
fn record_structured_checkpoint(
    authority: &SqliteTaskAuthority,
    directory: &CellDirectory,
    cell: CellIdentity,
) -> nlos_cell::CheckpointRecord {
    let durable = durable_commit_prefix(authority, task_id()).expect("derive durable prefix");
    let digest = CheckpointDigest::from_prefix(
        core_generation(),
        core_epoch(),
        core_token(),
        durable.prefix(),
    );
    let fact = CheckpointFact::new(
        core_generation(),
        core_epoch(),
        core_token(),
        &digest.to_text(),
    )
    .expect("structured checkpoint fact");
    directory
        .record_checkpoint(cell, &fact)
        .expect("record the structured checkpoint")
}

fn converge<'a>(
    authority: &'a SqliteTaskAuthority,
    directory: &'a CellDirectory,
    cell: CellIdentity,
    lease: &nlos_task::AuthorityLeaseRecord,
) -> Result<nlos_slice_k::ReconciliationOutcome, ReconciliationError> {
    converge_commit_prefix_gap(ReconciliationRequest {
        store: authority,
        directory,
        authority_cell: cell,
        task_id: task_id(),
        term_lease: *lease,
    })
}

fn assert_invariants_all_hold(invariants: &ConvergenceInvariants) {
    assert!(
        invariants.crash_window_converged,
        "invariant 1 (crash-window convergence)"
    );
    assert!(invariants.no_phantom_rows, "invariant 2 (no phantom rows)");
    assert!(
        invariants.no_double_commit,
        "invariant 3 (no double commit)"
    );
    assert_ne!(
        invariants.replay_digest, [0; 32],
        "invariant 4 (replay digest) is computed"
    );
}

/// Scenario: gap coverage + idempotent replay. The trail's verified
/// anchor covers the first segment; the second segment is the crash-window
/// gap; convergence adopts exactly the gap and lands on the durable
/// prefix as the unique final state; an identical rerun produces a
/// byte-identical record.
#[test]
fn gap_converges_from_verified_anchor_and_replay_is_byte_identical() {
    let fixture = core_fixture("gap", 0x91);
    let cell = synthetic_cell(0xA1);
    commit_segment(&fixture.authority, &fixture.lease, 0x21, 0, 110, 2);
    record_structured_checkpoint(&fixture.authority, &fixture.directory, cell);
    commit_segment(&fixture.authority, &fixture.lease, 0x22, 16, 120, 2);

    let first = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect("convergence succeeds over the gap");
    assert_eq!(
        first.covered_entries, 2,
        "the anchor covers exactly the first segment"
    );
    assert_eq!(first.visibility, PrefixVisibility::Partial);
    assert_eq!(
        first
            .adopted
            .iter()
            .map(|entry| entry.effect_history_seq)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "the gap adopts exactly the second segment's durable entries"
    );
    assert_eq!(first.final_state.entry_count, 4);
    assert_eq!(first.final_state.head_commit_seq, 2, "two segment commits");
    assert_eq!(
        first.final_state.durable_prefix_root,
        fixture
            .authority
            .compute_effect_history_root(task_id())
            .expect("store root"),
        "the final state is the store's own durable prefix root"
    );
    assert_invariants_all_hold(&first.invariants);
    assert_eq!(first.trail.len(), 1);
    assert!(matches!(
        first.trail[0].cover,
        TrailCover::Anchored { covered_entries: 2 }
    ));

    // Idempotent replay: byte-identical record, structural equality, and
    // the same canonical replay digest.
    let second = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect("replay convergence succeeds");
    assert_eq!(first, second, "replay converges to the identical record");
    assert_eq!(
        first.invariants.replay_digest, second.invariants.replay_digest,
        "replay digests compare byte-exactly"
    );
}

/// Scenario: no double commit (behavioral path) + one final state from
/// different anchors. After the first convergence adopts the gap, a new
/// checkpoint covering everything makes the second convergence adopt
/// nothing while landing on the same unique final state.
#[test]
fn anchor_advance_adopts_nothing_and_keeps_the_final_state() {
    let fixture = core_fixture("anchor", 0x92);
    let cell = synthetic_cell(0xA2);
    commit_segment(&fixture.authority, &fixture.lease, 0x31, 0, 110, 2);
    record_structured_checkpoint(&fixture.authority, &fixture.directory, cell);
    commit_segment(&fixture.authority, &fixture.lease, 0x32, 16, 120, 2);

    let first = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect("first convergence adopts the gap");
    assert_eq!(first.covered_entries, 2);
    assert_eq!(
        first.adopted.len(),
        2,
        "the first convergence adopts the gap once"
    );

    record_structured_checkpoint(&fixture.authority, &fixture.directory, cell);
    let second = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect("second convergence after the anchor advanced");
    assert_eq!(
        second.covered_entries, 4,
        "the advanced anchor covers everything"
    );
    assert_eq!(
        second.visibility,
        PrefixVisibility::Covered,
        "no uncovered window remains"
    );
    assert!(
        second.adopted.is_empty(),
        "already-converged entries are never adopted a second time"
    );
    assert_eq!(
        second.final_state, first.final_state,
        "both convergence entries land on the same unique final state"
    );
    assert_invariants_all_hold(&second.invariants);
    assert_eq!(second.trail.len(), 2);
}

/// Scenario: different convergence entries — no verifiable anchor at all
/// (a Cell with an empty trail) versus a verified anchor — converge the
/// same durable prefix to the same unique final state.
#[test]
fn no_anchor_entry_reaches_the_same_final_state() {
    let fixture = core_fixture("entries", 0x93);
    let anchored_cell = synthetic_cell(0xA3);
    let trailless_cell = synthetic_cell(0xA4);
    commit_segment(&fixture.authority, &fixture.lease, 0x41, 0, 110, 2);
    record_structured_checkpoint(&fixture.authority, &fixture.directory, anchored_cell);
    commit_segment(&fixture.authority, &fixture.lease, 0x42, 16, 120, 2);

    let anchored = converge(
        &fixture.authority,
        &fixture.directory,
        anchored_cell,
        &fixture.lease,
    )
    .expect("entry with a verified anchor");
    let trailless = converge(
        &fixture.authority,
        &fixture.directory,
        trailless_cell,
        &fixture.lease,
    )
    .expect("entry with no verifiable anchor");
    assert_eq!(anchored.covered_entries, 2);
    assert_eq!(anchored.visibility, PrefixVisibility::Partial);
    assert_eq!(
        trailless.covered_entries, 0,
        "an empty trail verifies nothing"
    );
    assert_eq!(
        trailless.visibility,
        PrefixVisibility::Uncertain,
        "R1.2.3 honest window: nothing is trail-covered"
    );
    assert_eq!(
        trailless
            .adopted
            .iter()
            .map(|entry| entry.effect_history_seq)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "the whole durable prefix is the gap when nothing is covered"
    );
    assert_eq!(
        trailless.final_state, anchored.final_state,
        "both entries land on the same unique final state (bounded convergence)"
    );
    assert_invariants_all_hold(&trailless.invariants);
    assert_eq!(
        trailless.trail.len(),
        0,
        "the trailless Cell holds no records"
    );
}

/// Scenario: a tampered structured claim — well-formed digest over a
/// prefix the authority never durably made — fails closed as a phantom
/// claim, never converging on it.
#[test]
fn tampered_digest_claim_fails_closed() {
    let fixture = core_fixture("tamper", 0x94);
    let cell = synthetic_cell(0xA5);
    commit_segment(&fixture.authority, &fixture.lease, 0x51, 0, 110, 2);

    // The claim: the durable prefix plus one phantom entry.
    let durable = durable_commit_prefix(&fixture.authority, task_id()).expect("durable prefix");
    let mut phantom_entries = durable.prefix().entries().to_vec();
    phantom_entries.push(b"phantom-commit-entry".to_vec());
    let phantom_prefix = nlos_cell::CheckpointPrefix::from_entries(phantom_entries);
    let digest = CheckpointDigest::from_prefix(
        core_generation(),
        core_epoch(),
        core_token(),
        &phantom_prefix,
    );
    let fact = CheckpointFact::new(
        core_generation(),
        core_epoch(),
        core_token(),
        &digest.to_text(),
    )
    .expect("tampered fact is storable");
    fixture
        .directory
        .record_checkpoint(cell, &fact)
        .expect("record the tampered claim");

    let error = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect_err("a phantom claim must fail closed");
    let ReconciliationError::TrailClaimNotDerivable {
        presented,
        recomputed,
        ..
    } = &error
    else {
        panic!("expected a phantom-claim refusal, got {error:?}");
    };
    assert_ne!(
        presented, recomputed,
        "the claim differs from every derivable digest"
    );
}

/// Scenario: a corrupt structured body — tagged as `v1:sha256:` but not
/// decodable — fails closed and is never re-read as free text.
#[test]
fn corrupt_digest_body_fails_closed() {
    let fixture = core_fixture("corrupt", 0x95);
    let cell = synthetic_cell(0xA6);
    commit_segment(&fixture.authority, &fixture.lease, 0x61, 0, 110, 2);
    let fact = CheckpointFact::new(
        core_generation(),
        core_epoch(),
        core_token(),
        "v1:sha256:not-hex-at-all-0000000000000000000000000000000000000000zz",
    )
    .expect("the corrupt body passes the storage validation (it is just text)");
    fixture
        .directory
        .record_checkpoint(cell, &fact)
        .expect("record the corrupt claim");

    let error = converge(&fixture.authority, &fixture.directory, cell, &fixture.lease)
        .expect_err("a corrupt structured body must fail closed");
    assert!(
        matches!(error, ReconciliationError::TrailDigestCorrupt { .. }),
        "expected a corrupt-digest refusal, got {error:?}"
    );
}

/// Scenario: free-text digests cover nothing (R1.2.3) — `PARTIAL` when a
/// structured anchor coexists, `UNCERTAIN` when only free text exists —
/// and convergence still lands on the durable prefix.
#[test]
fn free_text_checkpoints_cover_nothing() {
    let fixture = core_fixture("freetext", 0x96);
    let partial_cell = synthetic_cell(0xA7);
    let uncertain_cell = synthetic_cell(0xA8);
    commit_segment(&fixture.authority, &fixture.lease, 0x71, 0, 110, 2);
    record_structured_checkpoint(&fixture.authority, &fixture.directory, partial_cell);
    commit_segment(&fixture.authority, &fixture.lease, 0x72, 16, 120, 2);
    for cell in [partial_cell, uncertain_cell] {
        let free_text = CheckpointFact::new(
            core_generation(),
            core_epoch(),
            core_token(),
            "author free-text high-water summary: nothing structured",
        )
        .expect("free-text fact");
        fixture
            .directory
            .record_checkpoint(cell, &free_text)
            .expect("record the free-text checkpoint");
    }

    let partial = converge(
        &fixture.authority,
        &fixture.directory,
        partial_cell,
        &fixture.lease,
    )
    .expect("convergence with one free-text record alongside the anchor");
    assert_eq!(
        partial.covered_entries, 2,
        "the free-text record extends no coverage"
    );
    assert_eq!(partial.visibility, PrefixVisibility::Partial);
    assert_eq!(partial.trail.len(), 2);
    assert!(matches!(partial.trail[1].cover, TrailCover::FreeText));
    assert_invariants_all_hold(&partial.invariants);

    let uncertain = converge(
        &fixture.authority,
        &fixture.directory,
        uncertain_cell,
        &fixture.lease,
    )
    .expect("convergence with only free-text records");
    assert_eq!(uncertain.covered_entries, 0);
    assert_eq!(uncertain.visibility, PrefixVisibility::Uncertain);
    assert_eq!(
        uncertain.adopted.len(),
        4,
        "the whole prefix is the uncovered window"
    );
    assert_eq!(
        uncertain.final_state, partial.final_state,
        "free text changes no final state"
    );
}

/// Scenario: the stale-term guard — a lease that does not bind the active
/// assignment is refused before any trail fact is read (the dual-primary
/// entry guard; the H6 matrix owns the real injection).
#[test]
fn stale_term_lease_is_refused() {
    let fixture = core_fixture("stale", 0x97);
    let cell = synthetic_cell(0xA9);
    commit_segment(&fixture.authority, &fixture.lease, 0x81, 0, 110, 2);
    record_structured_checkpoint(&fixture.authority, &fixture.directory, cell);

    let mut stale = fixture.lease;
    stale.term += 1;
    stale.fencing_token = [stale.term.to_be_bytes()[0]; 32];
    let error = converge(&fixture.authority, &fixture.directory, cell, &stale)
        .expect_err("a foreign lease must be refused");
    assert!(
        matches!(
            &error,
            ReconciliationError::NotTermHolder {
                presented_term: 2,
                active_term: 1
            }
        ),
        "expected the stale-term refusal, got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Dual-Cell evidence (single host, two OS processes).
// ---------------------------------------------------------------------------

const STORE_PATH_ENV: &str = "NLOS_CS0C_STORE_PATH";
const IDENTITY_ROOT_ENV: &str = "NLOS_CS0C_IDENTITY_ROOT";
const FEDERATION_ROOT_ENV: &str = "NLOS_CS0C_FEDERATION_ROOT";
const CELL_ROOT_ENV: &str = "NLOS_CS0C_CELL_ROOT";
const OUT_ENV: &str = "NLOS_CS0C_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0C_SIGN_REQUEST";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0C_SIGN_RESPONSE";
const REFUSE_ENV: &str = "NLOS_CS0C_REFUSE";
const RELEASE_ENV: &str = "NLOS_CS0C_RELEASE";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);

/// Logical clock of the run. Cell A's term-1 lease is requested at 100
/// with TTL 100 (expires at 200); its two commit segments and checkpoint
/// run inside that window; the successor's clock starts after the expiry
/// so the lease takeover is the legal expired-incumbent path.
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const SEGMENT_ONE_AT_MS: i64 = 110;
const SEGMENT_TWO_AT_MS: i64 = 130;
const PARENT_CLOCK_BASE_MS: i64 = 200;
const PARENT_CLOCK_STEP_MS: i64 = 10;
const PARENT_LEASE_TTL_MS: i64 = 100_000;

const CHILD_SEED: u8 = 0xC3;
const CHILD_HOLDER_TAG: u8 = 0xB3;
const PARENT_HOLDER_TAG: u8 = 0xB4;
const SIGNER_SEED: u8 = 0xA3;

fn child_domain() -> SchedulerDomainId {
    SchedulerDomainId::from_bytes([0xe3; 16])
}

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("threshold ordering")
}

/// Mirrors the store's private participant wire codes (1..=8) so the
/// fixture's deterministic endpoint-receipt derivation can hash the same
/// participant axes the signature message binds.
fn participant_wire(participant: nlos_task::ParticipantRecord) -> u8 {
    use nlos_task::ParticipantType::*;
    match participant.participant_type {
        TaskStore => 1,
        ArtifactHead => 2,
        SemanticAdmission => 3,
        ChannelTopic => 4,
        DriverGateway => 5,
        ResourceLedger => 6,
        ProcessBinding => 7,
        OperationBinding => 8,
    }
}

fn poc_remote_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &nlos_task::ParticipantRecord,
) -> ReceiptId {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"llmos/cs0c-dual-cell/remote-receipt/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire(*participant)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("remote receipt prefix"))
}

fn poc_barrier_digest(
    takeover_receipt_id: ReceiptId,
    participant: &nlos_task::ParticipantRecord,
) -> [u8; 32] {
    let mut hasher = sha2::Sha256::new();
    hasher.update(b"llmos/cs0c-dual-cell/barrier-digest/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire(*participant)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    hasher.finalize().into()
}

/// Atomically (temp file + rename) publishes `payload` at `path`.
fn publish(path: &Path, payload: &str) {
    let mut staging = path.as_os_str().to_os_string();
    staging.push(".tmp");
    let staging = PathBuf::from(staging);
    fs::write(&staging, payload).expect("write staging file");
    fs::rename(&staging, path).expect("publish file atomically");
}

fn append_line(path: &Path, line: &str) {
    use std::io::Write as _;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open receipt file for append");
    writeln!(file, "{line}").expect("append receipt line");
}

fn wait_while_alive(child: &mut Child, what: &str, path: &Path, needle: &str) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

fn wait_for_file_while_alive(child: &mut Child, what: &str, path: &Path) -> String {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Harness-only entry: Cell A, the old authority. Holds the term-1 lease,
/// commits segment one, registers its structured checkpoint through its
/// federated [`nlos_slice_k::CellHost`] wiring, commits segment two (the
/// crash-window gap), bootstraps the barrier signer, serves signed barrier
/// observations for the successor's takeover drive, then — while still
/// alive — proves its own convergence attempt under the stale term-1
/// lease is refused.
#[test]
#[ignore = "spawned by successor_converges_gap_after_takeover_while_old_cell_is_refused"]
#[allow(clippy::too_many_lines)]
fn child_old_authority_cell_helper() {
    let (
        Ok(store_path),
        Ok(identity_root),
        Ok(federation_root),
        Ok(cell_root),
        Ok(out_path),
        Ok(sign_request),
        Ok(sign_response),
        Ok(refuse),
        Ok(release),
    ) = (
        std::env::var(STORE_PATH_ENV),
        std::env::var(IDENTITY_ROOT_ENV),
        std::env::var(FEDERATION_ROOT_ENV),
        std::env::var(CELL_ROOT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
        std::env::var(REFUSE_ENV),
        std::env::var(RELEASE_ENV),
    )
    else {
        eprintln!("skipped: cs0c child env unset (harness entry, parent spawn only)");
        return;
    };
    let (sign_request, sign_response) = (Path::new(&sign_request), Path::new(&sign_response));

    // --- Cell A establishes its authority term and commits segment one.
    let authority = SqliteTaskAuthority::open(&store_path).expect("cell A opens its task store");
    let lease_one = match authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
            idempotency_key: IdempotencyKey::from_bytes([CHILD_SEED; 16]),
            requested_at_ms: LEASE_ONE_AT_MS,
            ttl_ms: LEASE_ONE_TTL_MS,
        })
        .expect("cell A acquires its term-1 lease")
    {
        AuthorityLeaseDecision::Acquired(record) => record,
        other => panic!("expected a fresh acquire, got {other:?}"),
    };
    assert_eq!(lease_one.term, 1);
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: LEASE_ONE_AT_MS + 1,
        })
        .expect("cell A registers the task");
    commit_segment(&authority, &lease_one, 0x24, 0, SEGMENT_ONE_AT_MS, 2);

    // --- Cell A registers its structured checkpoint through its own
    // federated CellHost wiring: the digest pins its durable prefix at
    // its own fence axes, derived through the same public grammar the
    // executor verifies with.
    let host = nlos_slice_k::CellHost::open_federated(
        Path::new(&cell_root),
        nlos_slice_k::CellHostConfig::new(
            child_domain(),
            700,
            300,
            DeviceId::from_bytes([0xe3; 16]),
            detector_config(),
        ),
        Path::new(&federation_root),
    )
    .expect("cell A opens its federated CellHost");
    let fence = host.fence();
    let identity = fence.identity();
    let durable = durable_commit_prefix(&authority, task_id()).expect("cell A derives its prefix");
    assert_eq!(durable.entry_count(), 2, "segment one holds two entries");
    let checkpoint_digest = CheckpointDigest::from_prefix(
        fence.node_boot_generation(),
        fence.epoch(),
        fence.fencing_token(),
        durable.prefix(),
    );
    let recorded = host
        .record_reconciliation_checkpoint(&checkpoint_digest.to_text())
        .expect("cell A records its checkpoint")
        .expect("federated host must record");
    assert_eq!(recorded.cell(), identity);
    assert!(recorded.fact().digest().starts_with("v1:sha256:"));

    // --- The crash-window gap: segment two commits after the checkpoint.
    commit_segment(&authority, &lease_one, 0x25, 16, SEGMENT_TWO_AT_MS, 2);

    // --- The barrier signer of this Cell's endpoints, in the shared
    // identity authority both processes read.
    let identity_authority =
        IdentityAuthority::open(&identity_root).expect("cell A opens identity root");
    let signing_key = SigningKey::from_bytes(&[SIGNER_SEED; 32]);
    let binding = identity_authority
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [SIGNER_SEED.wrapping_add(1); 32],
            control_domain_policy_digest: [SIGNER_SEED.wrapping_add(2); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000_000,
            idempotency_key: IdempotencyKey::from_bytes([SIGNER_SEED.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap the barrier signer")
        .binding();

    publish(
        Path::new(&out_path),
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

    // --- Serve signed barrier observations until the refusal stage.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&refuse).exists() {
        assert!(
            Instant::now() < deadline,
            "cell A timed out waiting for the refusal stage"
        );
        let Ok(request) = fs::read_to_string(sign_request) else {
            thread::sleep(POLL);
            continue;
        };
        fs::remove_file(sign_request).expect("consume sign request");
        let mut lines = request.lines();
        let takeover_receipt_id =
            ReceiptId::from_bytes(fixed_hex(lines.next().expect("takeover receipt line")));
        let participant_type_code = lines
            .next()
            .expect("participant type line")
            .parse::<u8>()
            .expect("participant type code");
        let participant = nlos_task::ParticipantRecord {
            participant_type: match participant_type_code {
                1 => nlos_task::ParticipantType::TaskStore,
                2 => nlos_task::ParticipantType::ArtifactHead,
                3 => nlos_task::ParticipantType::SemanticAdmission,
                4 => nlos_task::ParticipantType::ChannelTopic,
                5 => nlos_task::ParticipantType::DriverGateway,
                6 => nlos_task::ParticipantType::ResourceLedger,
                7 => nlos_task::ParticipantType::ProcessBinding,
                8 => nlos_task::ParticipantType::OperationBinding,
                other => panic!("unknown participant type code {other}"),
            },
            participant_id: nlos_types::TaskParticipantId::from_bytes(fixed_hex(
                lines.next().expect("participant id line"),
            )),
            participant_generation: Generation::new(
                NonZeroU64::new(
                    lines
                        .next()
                        .expect("participant generation line")
                        .parse::<u64>()
                        .expect("participant generation"),
                )
                .expect("nonzero generation"),
            ),
            admission_receipt_id: ReceiptId::from_bytes(fixed_hex(
                lines.next().expect("admission receipt line"),
            )),
        };
        let fence_set_root = fixed_hex(lines.next().expect("fence root line"));
        let remote_receipt_id = poc_remote_receipt_id(takeover_receipt_id, &participant);
        let barrier_digest = poc_barrier_digest(takeover_receipt_id, &participant);
        let message = nlos_task::barrier_observation_signature_message(
            takeover_receipt_id,
            &participant,
            remote_receipt_id,
            barrier_digest,
            fence_set_root,
        );
        let signature = signing_key.sign(&message).to_bytes();
        publish(
            sign_response,
            &format!(
                "{}\n{}\n{}\n",
                hex(remote_receipt_id.as_bytes()),
                hex(barrier_digest.as_slice()),
                hex(signature.as_slice()),
            ),
        );
    }

    // --- Refusal stage: the still-live old Cell converges under its
    // stale term-1 lease and must be refused at the entry guard.
    let directory = host
        .cell_directory()
        .expect("federated host carries the directory");
    let refused = converge(&authority, directory, identity, &lease_one);
    match &refused {
        Err(ReconciliationError::NotTermHolder {
            presented_term: 1,
            active_term: 2,
        }) => {}
        other => panic!("expected the stale-term refusal, got {other:?}"),
    }
    append_line(Path::new(&out_path), "converge_refused:not_term_holder");
    append_line(Path::new(&out_path), "refusal_ok");

    // Stay alive until the successor releases — live dual-Cell overlap.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&release).exists() {
        assert!(
            Instant::now() < deadline,
            "cell A timed out waiting for release"
        );
        thread::sleep(POLL);
    }
}

/// The harness temp-file channel set shared with the spawned old-Cell
/// process: the receipt file, the sign request/response pair, and the
/// refuse/release stage flags.
struct ChildChannels {
    out: PathBuf,
    sign_request: PathBuf,
    sign_response: PathBuf,
    refuse: PathBuf,
    release: PathBuf,
}

impl ChildChannels {
    fn new(stamp: u32) -> Self {
        let base = std::env::temp_dir();
        Self {
            out: base.join(format!("nlos-slice-k-cs0c-out-{stamp}.txt")),
            sign_request: base.join(format!("nlos-slice-k-cs0c-req-{stamp}.txt")),
            sign_response: base.join(format!("nlos-slice-k-cs0c-resp-{stamp}.txt")),
            refuse: base.join(format!("nlos-slice-k-cs0c-refuse-{stamp}.flag")),
            release: base.join(format!("nlos-slice-k-cs0c-release-{stamp}.flag")),
        }
    }

    fn paths(&self) -> [&Path; 5] {
        [
            &self.out,
            &self.sign_request,
            &self.sign_response,
            &self.refuse,
            &self.release,
        ]
    }

    fn clear_all(&self) {
        for path in self.paths() {
            let _ = fs::remove_file(path);
        }
    }
}

fn spawn_child(
    store_path: &Path,
    identity_root: &Path,
    federation_root: &Path,
    cell_root: &Path,
    channels: &ChildChannels,
) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_old_authority_cell_helper",
            "--nocapture",
            "--ignored",
        ])
        .env(STORE_PATH_ENV, store_path)
        .env(IDENTITY_ROOT_ENV, identity_root)
        .env(FEDERATION_ROOT_ENV, federation_root)
        .env(CELL_ROOT_ENV, cell_root)
        .env(OUT_ENV, &channels.out)
        .env(SIGN_REQUEST_ENV, &channels.sign_request)
        .env(SIGN_RESPONSE_ENV, &channels.sign_response)
        .env(REFUSE_ENV, &channels.refuse)
        .env(RELEASE_ENV, &channels.release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the old-authority Cell process")
}

/// Parent-side endpoint channel: carries the successor's barrier demand
/// to the still-live old Cell over the harness temp files and returns its
/// signed observation. Signer identity fields come from the child's
/// registration receipt, never from the response payload.
struct FileSignChannel<'a> {
    child: &'a mut Child,
    request_path: PathBuf,
    response_path: PathBuf,
    issuer: PrincipalId,
    control_domain_id: ControlDomainId,
    key_id: KeyId,
}

impl BarrierObservationSource for FileSignChannel<'_> {
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        let _ = fs::remove_file(&self.response_path);
        let participant = demand.participant;
        let payload = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            hex(demand.takeover_receipt_id.as_bytes()),
            participant_wire(participant),
            hex(participant.participant_id.as_bytes()),
            participant.participant_generation.get(),
            hex(participant.admission_receipt_id.as_bytes()),
            hex(demand.fence_set_root.as_slice()),
        );
        fs::write(&self.request_path, payload)
            .map_err(|error| TakeoverDriveError::BarrierSource(format!("sign request: {error}")))?;
        let response = wait_for_file_while_alive(
            self.child,
            "signed barrier observation",
            &self.response_path,
        );
        let mut lines = response.lines();
        let remote_receipt_id =
            ReceiptId::from_bytes(fixed_hex(lines.next().expect("remote receipt line")));
        let barrier_digest: [u8; 32] = fixed_hex(lines.next().expect("barrier digest line"));
        let signature: [u8; 64] = fixed_hex(lines.next().expect("signature line"));
        fs::remove_file(&self.response_path).map_err(|error| {
            TakeoverDriveError::BarrierSource(format!("consume response: {error}"))
        })?;
        Ok(SignedBarrierObservation {
            remote_receipt_id,
            barrier_digest,
            signature: nlos_task::BarrierObservationSignature {
                issuer: self.issuer,
                control_domain_id: self.control_domain_id,
                key_id: self.key_id,
                signature,
            },
        })
    }
}

/// The successor Cell: drives the full CS0-A takeover over the old Cell's
/// still-open task store, converges the checkpoint gap, replays the
/// convergence byte-exactly, converges from a second entry with no
/// verified anchor, and receipts the still-live old Cell's stale-term
/// refusal.
#[test]
#[allow(clippy::too_many_lines)]
fn successor_converges_gap_after_takeover_while_old_cell_is_refused() {
    let stamp = std::process::id();
    let store_dir = TempDir::new("store");
    let identity_dir = TempDir::new("identity");
    let child_cell_dir = TempDir::new("child-cell");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let channels = ChildChannels::new(stamp);
    channels.clear_all();

    let mut child = spawn_child(
        &store_path,
        identity_dir.path(),
        store_dir.path(), // the federation root lives beside the store
        child_cell_dir.path(),
        &channels,
    );

    // Stage 1: the old-authority Cell registered its term, checkpoint,
    // and signer.
    let registration = wait_for_file_while_alive(&mut child, "child registration", &channels.out);
    let mut lines = registration.lines();
    assert_eq!(lines.next(), Some("registered"));
    let child_pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    let old_cell = CellIdentity::from_domain(SchedulerDomainId::from_bytes(fixed_hex(
        lines.next().expect("identity line"),
    )));
    let checkpoint_text = lines.next().expect("checkpoint digest line").to_string();
    let issuer = PrincipalId::from_bytes(fixed_hex(lines.next().expect("principal line")));
    let control_domain_id =
        ControlDomainId::from_bytes(fixed_hex(lines.next().expect("domain line")));
    let key_id = KeyId::from_bytes(fixed_hex(lines.next().expect("key line")));
    assert_ne!(child_pid, std::process::id());
    assert_eq!(
        old_cell,
        CellIdentity::from_domain(child_domain()),
        "the old authority must be the agreed second Cell"
    );
    let parent_holder = holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);

    // Stage 2: the successor drives the full takeover chain (CS0-A).
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let pre_lease = successor_store
        .inspect_authority_lease()
        .expect("read the incumbent lease");
    assert_eq!(pre_lease.term, 1);
    let identity =
        IdentityAuthority::open(identity_dir.path()).expect("successor opens identity root");
    let mut source = FileSignChannel {
        child: &mut child,
        request_path: channels.sign_request.clone(),
        response_path: channels.sign_response.clone(),
        issuer,
        control_domain_id,
        key_id,
    };
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
        lease_idempotency_key: IdempotencyKey::from_bytes([0x43; 16]),
        lease_ttl_ms: PARENT_LEASE_TTL_MS,
        now_ms: &mut clock,
    })
    .expect("the successor drives the full takeover chain");
    drop(source);
    assert_eq!(takeover.successor_lease.term, 2);
    assert!(
        child.try_wait().expect("child alive at takeover").is_none(),
        "the old Cell must still be live when the takeover completes"
    );

    // Stage 3: convergence of the checkpoint gap. The trail's verified
    // anchor covers segment one (two entries); segment two is the
    // crash-window gap.
    let directory =
        CellDirectory::open(store_dir.path()).expect("successor opens the shared federation root");
    let outcome = converge(
        &successor_store,
        &directory,
        old_cell,
        &takeover.successor_lease,
    )
    .expect("the successor converges the checkpoint gap");
    assert_eq!(outcome.term, 2);
    assert_eq!(outcome.holder_id, parent_holder);
    assert_eq!(outcome.covered_entries, 2);
    assert_eq!(outcome.visibility, PrefixVisibility::Partial);
    let anchor = outcome.anchor.as_ref().expect("a verified anchor exists");
    assert_eq!(anchor.covered_entries, 2);
    assert_eq!(
        anchor.digest.to_text(),
        checkpoint_text,
        "the anchor is exactly the old Cell's registered structured digest"
    );
    assert_eq!(anchor.os_process_id, child_pid);
    assert_eq!(
        outcome
            .adopted
            .iter()
            .map(|entry| entry.effect_history_seq)
            .collect::<Vec<_>>(),
        vec![3, 4],
        "the gap adopts exactly the post-checkpoint durable entries"
    );
    assert_eq!(outcome.final_state.entry_count, 4);
    assert_eq!(outcome.final_state.head_commit_seq, 2);
    assert_eq!(
        outcome.final_state.durable_prefix_root,
        successor_store
            .compute_effect_history_root(task_id())
            .expect("store root"),
        "the final state is the store's own durable prefix root"
    );
    assert_invariants_all_hold(&outcome.invariants);
    assert_eq!(outcome.trail.len(), 1);
    assert!(matches!(
        outcome.trail[0].cover,
        TrailCover::Anchored { covered_entries: 2 }
    ));

    // Stage 4: idempotent replay — byte-identical record.
    let replay = converge(
        &successor_store,
        &directory,
        old_cell,
        &takeover.successor_lease,
    )
    .expect("replay convergence succeeds");
    assert_eq!(outcome, replay, "replay converges to the identical record");
    assert_eq!(
        outcome.invariants.replay_digest,
        replay.invariants.replay_digest
    );

    // Stage 5: a different convergence entry — a Cell with no trail —
    // reaches the same unique final state (bounded convergence).
    let trailless_cell = synthetic_cell(0xB5);
    let trailless = converge(
        &successor_store,
        &directory,
        trailless_cell,
        &takeover.successor_lease,
    )
    .expect("entry with no verifiable anchor");
    assert_eq!(trailless.covered_entries, 0);
    assert_eq!(trailless.visibility, PrefixVisibility::Uncertain);
    assert_eq!(trailless.adopted.len(), 4);
    assert_eq!(
        trailless.final_state, outcome.final_state,
        "both convergence entries land on the same unique final state"
    );
    assert_invariants_all_hold(&trailless.invariants);

    // Stage 6: the still-live old Cell proves its stale-term convergence
    // is refused.
    fs::write(&channels.refuse, b"refuse").expect("signal the refusal stage");
    wait_while_alive(
        &mut child,
        "old Cell refusal receipt",
        &channels.out,
        "refusal_ok",
    );
    let receipts = fs::read_to_string(&channels.out).expect("read child receipts");
    assert!(receipts.contains("converge_refused:not_term_holder"));
    assert!(
        child
            .try_wait()
            .expect("child alive at refusal evidence")
            .is_none(),
        "both Cells must be live when the refusal evidence is cited"
    );

    fs::write(&channels.release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "old Cell process failed: {status:?}");

    channels.clear_all();
}
