//! CS0-D shared fault-harness support (H6 fault-injection matrix). This
//! module is compiled into every `dual_cell_takeover_fault_*` test binary
//! via `mod common;` and is never a test target itself.
//!
//! # Injection-channel design decision (C-SHARD plan R7, recorded here per
//! the lane brief)
//!
//! The dual-Cell convention (ADR-0018: one OS process = one Cell) carries
//! the successor's barrier demand to the old Cell over a temp-file
//! request/response pair wrapped in a [`BarrierObservationSource`]
//! implementation — that public trait seam is the Phase 0 "communication
//! channel". R7 records that no process channel existed to cut; this
//! module is the minimal channel mechanism layered on the seam, with no
//! production-code hook at all (`takeover_drive.rs` / `reconciliation.rs`
//! are untouched by CS0-D):
//!
//! - **Parent-side cut** ([`CuttableSignChannel::cut`]): while set, the
//!   source refuses to deliver the demand — no request file is written —
//!   and fails closed with [`TakeoverDriveError::BarrierSource`], the
//!   drive's own fail-closed channel error. The old Cell provably
//!   receives nothing: its served-demand counter never advances.
//! - **Child-side block** (`block` flag file): while present, the old
//!   Cell refuses to answer any demand that does arrive (it neither
//!   consumes the request nor answers) — the same refusal enforced at
//!   the endpoint half of the channel.
//! - **Liveness proof, cut ≠ death** (the methodological hard
//!   constraint): the old Cell runs a heartbeat thread that appends a
//!   monotonically increasing `hb <n>` line to a dedicated heartbeat
//!   file — a file outside the blocked demand channel — every 50 ms
//!   until released. The parent reads the counter before/during/after
//!   every cut window and asserts it advanced, plus
//!   `child.try_wait().is_none()` at every evidence point, plus its own
//!   continued execution (it is the test process). Kill-based injection
//!   is reserved for the restart / migration-fault categories, where a
//!   crash is the point; the partition and dual-primary categories never
//!   signal or kill the child during their cut windows.
//!
//! # Honesty bound
//!
//! Everything here is single-host dual-process harness plumbing: shared
//! filesystem, shared kernel, shared clock domain (ADR-0019 R2.2 item 1).
//! Citing evidence produced through it as cross-host, network-partition,
//! or cross-clock evidence would violate RISK-B-11. The injected "cut" is
//! a delivery refusal at the harness channel, not an OS-level network
//! partition — the assertion it supports is that the takeover semantics
//! fail closed while the endpoint stays alive and unreachable, which is
//! the semantics a real partition exercises.

#![allow(dead_code)] // each fault binary uses a subset of the shared surface

use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::IdentityAuthority;
use nlos_slice_k::{
    BarrierObservationDemand, BarrierObservationSource, SignedBarrierObservation,
    TakeoverDriveError,
};
use nlos_task::{
    AttemptSpec, AuthorityLeaseRecord, AuthorityLeaseRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRecord,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverCompletionRecord,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    FinalizeRequest, FinalizeRequestV3, LogicalEffectDescriptor, PermitDecision, PermitRequest,
    PlannedEffect, SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError,
    barrier_observation_signature_message,
};
use nlos_types::{
    CancellationScopeId, ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId,
    ProcessId, ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use sha2::Digest as _;

/// Poll cadence of every file-flag wait loop (the `nlos-cell` convention).
pub(crate) const POLL: Duration = Duration::from_millis(10);
/// Wall-clock deadline every child stage honors.
pub(crate) const CHILD_WAIT: Duration = Duration::from_secs(30);
/// Heartbeat cadence of the old Cell's liveness thread.
pub(crate) const HEARTBEAT: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// Temp dirs, atomic publication, codec — the dual-Cell family conventions.
// ---------------------------------------------------------------------------

/// Per-run scratch directory (auto-removed on drop).
pub(crate) struct TempDir {
    root: PathBuf,
}

impl TempDir {
    pub(crate) fn new(name: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-cs0d-{name}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create cs0d temp root");
        Self { root }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove cs0d temp root: {error}"),
        }
    }
}

/// Atomically (temp file + rename) publishes `payload` at `path`, so the
/// polling reader never observes a half-written file.
pub(crate) fn publish(path: &Path, payload: &str) {
    let mut staging = path.as_os_str().to_os_string();
    staging.push(".tmp");
    let staging = PathBuf::from(staging);
    fs::write(&staging, payload).expect("write staging file");
    fs::rename(&staging, path).expect("publish file atomically");
}

/// Appends one receipt line to a stage receipt file.
pub(crate) fn append_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .expect("open receipt file for append");
    writeln!(file, "{line}").expect("append receipt line");
}

pub(crate) fn hex(bytes: &[u8]) -> String {
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

pub(crate) fn fixed_hex<const N: usize>(line: &str) -> [u8; N] {
    let bytes = line.as_bytes();
    assert_eq!(bytes.len(), N * 2, "hex line must be {N} bytes wide");
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[2 * index]) << 4) | hex_nibble(bytes[2 * index + 1]);
    }
    out
}

/// Deterministic holder identity from a real pid plus a lane tag byte.
pub(crate) fn holder_from_pid(pid: u32, tag: u8) -> ProcessId {
    let mut bytes = [0_u8; 16];
    bytes[..4].copy_from_slice(&pid.to_be_bytes());
    bytes[15] = tag;
    ProcessId::from_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Old-Cell baseline machinery: task/attempt/permit/finalize and the
// deterministic endpoint observation derivations (CS0-A/CS0-C conventions).
// ---------------------------------------------------------------------------

/// Mirrors the store's private participant wire codes (1..=8) so the
/// deterministic endpoint-receipt derivation hashes the same participant
/// axes the signature message binds.
pub(crate) fn participant_wire(participant: nlos_task::ParticipantRecord) -> u8 {
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

fn observation_axes(
    domain: &[u8],
    takeover_receipt_id: ReceiptId,
    participant: &nlos_task::ParticipantRecord,
    wide: bool,
) -> Vec<u8> {
    let mut hasher = sha2::Sha256::new();
    hasher.update(domain);
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire(*participant)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    if wide {
        digest.to_vec()
    } else {
        digest[..16].to_vec()
    }
}

/// Deterministic endpoint-durable receipt identity the old Cell mints for
/// one barrier observation.
pub(crate) fn poc_remote_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &nlos_task::ParticipantRecord,
) -> ReceiptId {
    let bytes = observation_axes(
        b"llmos/cs0d-dual-cell/remote-receipt/v1",
        takeover_receipt_id,
        participant,
        false,
    );
    ReceiptId::from_bytes(bytes[..16].try_into().expect("remote receipt prefix"))
}

/// Deterministic endpoint-authored barrier digest the old Cell binds into
/// its observation.
pub(crate) fn poc_barrier_digest(
    takeover_receipt_id: ReceiptId,
    participant: &nlos_task::ParticipantRecord,
) -> [u8; 32] {
    let bytes = observation_axes(
        b"llmos/cs0d-dual-cell/barrier-digest/v1",
        takeover_receipt_id,
        participant,
        true,
    );
    bytes[..32].try_into().expect("barrier digest width")
}

fn planned_effect(task_id: TaskId, slot_base: u64, slot: u64, seed: u8) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id,
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

/// Registers the task and one attempt (the CS0-C registration shape).
pub(crate) fn register_task_and_attempt(
    authority: &SqliteTaskAuthority,
    task_id: TaskId,
    seed: u8,
    at_ms: i64,
) -> AttemptSpec {
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: at_ms,
        })
        .expect("register task");
    let head = authority.inspect_task(task_id).expect("task head");
    let attempt = AttemptSpec {
        task_id,
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
        .expect("register attempt");
    attempt
}

/// Issues a lease-bound `CommitPermit` for one registered attempt with
/// `effects` declared (all optional) slots. Does not close slots and does
/// not finalize — the caller composes the commit chain shape it needs.
pub(crate) fn issue_lease_bound_permit(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    task_id: TaskId,
    seed: u8,
    slot_base: u64,
    at_ms: i64,
    effects: u64,
) -> (AttemptSpec, nlos_task::PermitRecord) {
    let attempt = register_task_and_attempt(authority, task_id, seed, at_ms);
    let permit = match authority
        .request_commit_permit_with_authority_lease(nlos_task::AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [seed; 32],
                planned_effects: (0..effects)
                    .map(|slot| planned_effect(task_id, slot_base, slot, seed))
                    .collect(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
                valid_until_ms: 1_000_000,
                requested_at_ms: at_ms + 1,
            },
            lease: *lease,
        })
        .expect("issue the lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    (attempt, permit)
}

/// Closes one declared effect slot completely (effect permit → dispatch
/// token → recorded outcome): the closure that appends the durable history
/// entry.
#[allow(clippy::too_many_arguments)]
pub(crate) fn close_effect_slot(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    task_id: TaskId,
    attempt: &AttemptSpec,
    permit: &nlos_task::PermitRecord,
    seed: u8,
    slot: u64,
    at_ms: i64,
) {
    consume_dispatch(
        authority, lease, task_id, attempt, permit, seed, slot, at_ms,
    )
    .expect("consume the dispatch token");
    record_outcome(authority, lease, attempt, permit, seed, slot, at_ms)
        .expect("close the effect slot");
}

/// Draws the effect permit and consumes its one-shot dispatch token,
/// leaving the outcome un-recorded (a mid-flight slot).
#[allow(clippy::too_many_arguments)]
pub(crate) fn consume_dispatch(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    task_id: TaskId,
    attempt: &AttemptSpec,
    permit: &nlos_task::PermitRecord,
    seed: u8,
    slot: u64,
    at_ms: i64,
) -> Result<nlos_task::IssuedPermit, TaskStoreError> {
    // Effect-permit idempotency keys namespace per task (cross attempt),
    // so the key must differ for every (attempt-seed, slot) pair.
    let slot_tag = u8::try_from(slot).expect("test slot fits u8");
    let mut effect_key = [0xEC_u8; 16];
    effect_key[1] = seed;
    effect_key[2] = slot_tag;
    let issued = match authority
        .request_effect_permit_with_authority_lease(nlos_task::AuthorityLeaseEffectPermitRequest {
            permit: nlos_task::EffectPermitRequest {
                task_id,
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
        nlos_task::EffectPermitDecision::Issued(record) => record,
        other @ nlos_task::EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    };
    authority.consume_dispatch_token_with_authority_lease(
        nlos_task::AuthorityLeaseDispatchRequest {
            dispatch: nlos_task::DispatchRequest {
                task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_permit_id: issued.effect_permit_id,
                dispatch_token: issued.one_shot_dispatch_token,
                dispatched_at_ms: at_ms + 3,
            },
            lease: *lease,
        },
    )?;
    Ok(*issued)
}

/// Records the outcome of one dispatched slot (the durable closure).
pub(crate) fn record_outcome(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    attempt: &AttemptSpec,
    permit: &nlos_task::PermitRecord,
    seed: u8,
    slot: u64,
    at_ms: i64,
) -> Result<nlos_task::EffectReceiptDecision, TaskStoreError> {
    let slot_tag = u8::try_from(slot).expect("test slot fits u8");
    authority.record_effect_outcome_with_authority_lease(nlos_task::AuthorityLeaseOutcomeRequest {
        outcome: nlos_task::OutcomeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: slot,
            outcome: nlos_task::Outcome::Closed {
                authoritative_closure_digest: [seed.wrapping_add(slot_tag); 32],
            },
            recorded_at_ms: at_ms + 4,
        },
        lease: *lease,
    })
}

/// Finalizes one permit's commit (advances the `TaskHead`).
pub(crate) fn finalize_commit(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    attempt: &AttemptSpec,
    permit: &nlos_task::PermitRecord,
    at_ms: i64,
) -> Result<nlos_task::FinalizeDecision, TaskStoreError> {
    authority.finalize_commit_v3_with_authority_lease(nlos_task::AuthorityLeaseFinalizeRequest {
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
}

/// Commits one prefix segment under the held lease (the CS0-C `commit_
/// segment` shape): permit with `effects` declared slots, every slot
/// closed, then the finalize.
pub(crate) fn commit_segment(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    task_id: TaskId,
    seed: u8,
    slot_base: u64,
    at_ms: i64,
    effects: u64,
) {
    let (attempt, permit) =
        issue_lease_bound_permit(authority, lease, task_id, seed, slot_base, at_ms, effects);
    for slot in 0..effects {
        close_effect_slot(
            authority, lease, task_id, &attempt, &permit, seed, slot, at_ms,
        );
    }
    finalize_commit(authority, lease, &attempt, &permit, at_ms)
        .expect("finalize the segment commit");
}

/// One mid-flight attempt for stale-submission scenarios: a single
/// outstanding permit (the task admits exactly one `Issued` permit at a
/// time) whose slot 0 is fully closed, whose slot 1 has its dispatch
/// token consumed but its outcome un-recorded (the `result`-class face),
/// and whose slot 2 never requested an effect permit (the `effect`-class
/// face). The permit itself stays unfinalized (the `commit`-class face:
/// the finalize gate's lease validation runs before its terminal-state
/// machinery, so a stale-lease finalize of this permit is refused by the
/// fence alone).
pub(crate) struct MidFlightAttempts {
    /// Attempt with in-flight slots.
    pub flighted: AttemptSpec,
    /// Permit of [`Self::flighted`] (still `Issued`).
    pub flighted_permit: nlos_task::PermitRecord,
    /// Slot whose dispatch is consumed and outcome pending.
    pub pending_outcome_slot: u64,
    /// Slot whose effect permit was never requested.
    pub unrequested_effect_slot: u64,
}

/// Builds the [`MidFlightAttempts`] state under the held (live) lease.
pub(crate) fn open_mid_flight_attempts(
    authority: &SqliteTaskAuthority,
    lease: &AuthorityLeaseRecord,
    task_id: TaskId,
    seed: u8,
    slot_base: u64,
    at_ms: i64,
) -> MidFlightAttempts {
    let declared = 3;
    let (flighted, flighted_permit) =
        issue_lease_bound_permit(authority, lease, task_id, seed, slot_base, at_ms, declared);
    close_effect_slot(
        authority,
        lease,
        task_id,
        &flighted,
        &flighted_permit,
        seed,
        0,
        at_ms,
    );
    consume_dispatch(
        authority,
        lease,
        task_id,
        &flighted,
        &flighted_permit,
        seed,
        1,
        at_ms,
    )
    .expect("consume the mid-flight dispatch");
    MidFlightAttempts {
        flighted,
        flighted_permit,
        pending_outcome_slot: 1,
        unrequested_effect_slot: 2,
    }
}

// ---------------------------------------------------------------------------
// Child side: spawn, heartbeat thread, the block-aware sign server.
// ---------------------------------------------------------------------------

/// Spawns one `#[ignore]` child helper of this test binary (the dual-Cell
/// convention: `current_exe` + `--exact` + env, no product IPC).
pub(crate) fn spawn_fault_child(helper: &str, envs: &[(&str, std::ffi::OsString)]) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", helper, "--nocapture", "--ignored"])
        .envs(
            envs.iter()
                .map(|(key, value)| (key.to_string(), value.clone())),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the child Cell process")
}

/// Old-Cell liveness thread: appends `hb <n>` (n increasing) to the
/// heartbeat file every 50 ms until the stop flag appears, then `hb_end`.
/// This is the file outside the blocked demand channel that proves the
/// child process stays alive (and executing) through every cut window.
pub(crate) fn spawn_heartbeat(heartbeat: &Path, stop: &Path) -> thread::JoinHandle<()> {
    let heartbeat = heartbeat.to_path_buf();
    let stop = stop.to_path_buf();
    thread::spawn(move || {
        let mut counter: u64 = 0;
        loop {
            if stop.exists() {
                append_line(&heartbeat, "hb_end");
                return;
            }
            counter += 1;
            append_line(&heartbeat, &format!("hb {counter}"));
            thread::sleep(HEARTBEAT);
        }
    })
}

/// The old Cell's endpoint half of the barrier channel: answers signed
/// observation demands over the temp-file pair until the stop flag
/// appears. While the block flag exists, every arriving demand is refused
/// (neither consumed nor answered) — the child-side half of the injected
/// cut. Every served demand is appended to the heartbeat file as
/// `demand <n>` so the parent can prove delivery counts.
pub(crate) fn serve_observations(
    sign_request: &Path,
    sign_response: &Path,
    stop: &Path,
    block: &Path,
    heartbeat: &Path,
    signing_key: &SigningKey,
) {
    let deadline = Instant::now() + CHILD_WAIT;
    let mut served: usize = 0;
    while !stop.exists() {
        assert!(
            Instant::now() < deadline,
            "old Cell timed out serving barrier observations"
        );
        let Ok(request) = fs::read_to_string(sign_request) else {
            thread::sleep(POLL);
            continue;
        };
        if block.exists() {
            // Refuse: leave the request unconsumed and unanswered. The
            // parent observes no response and no demand-count movement.
            thread::sleep(POLL);
            continue;
        }
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
        let message = barrier_observation_signature_message(
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
        served += 1;
        append_line(heartbeat, &format!("demand {served}"));
    }
}

/// Parses the last heartbeat counter of a heartbeat file.
///
/// A not-yet-created file reads as zero: the child may not have written its
/// first heartbeat when the parent's bounded wait loop polls, and that race
/// must keep polling (the loop's deadline bounds it), not panic.
pub(crate) fn heartbeat_counter(path: &Path) -> u64 {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!("read heartbeat file: {error}"),
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("hb ").and_then(|n| n.parse().ok()))
        .max()
        .unwrap_or(0)
}

/// Parses how many demands the child served so far (same not-yet-created
/// semantics as [`heartbeat_counter`]).
pub(crate) fn demand_count(path: &Path) -> usize {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!("read heartbeat file: {error}"),
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("demand ").and_then(|n| n.parse().ok()))
        .max()
        .unwrap_or(0)
}

/// Waits until the old Cell's heartbeat counter reaches `at_least`,
/// proving the child process is alive and executing. Panics on early
/// child exit or timeout.
pub(crate) fn wait_heartbeat_at_least(child: &mut Child, heartbeat: &Path, at_least: u64) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if heartbeat_counter(heartbeat) >= at_least {
            assert!(
                child.try_wait().expect("child alive").is_none(),
                "the old Cell must still be live while its heartbeat advances"
            );
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited while heartbeating: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the old Cell's heartbeat"
        );
        thread::sleep(POLL);
    }
}

/// Polls for `path` to exist and contain `needle` while the child is still
/// alive (the dual-Cell family convention).
pub(crate) fn wait_while_alive(child: &mut Child, what: &str, path: &Path, needle: &str) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Polls for the atomic publication at `path` while the child is alive and
/// returns its full content.
pub(crate) fn wait_for_file_while_alive(child: &mut Child, what: &str, path: &Path) -> String {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Force-terminates (SIGKILL) and reaps one child — the crash-injection
/// convention of the `nlos-task` fault matrix, legal only for the restart
/// and migration-fault categories.
pub(crate) fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("reap killed child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

// ---------------------------------------------------------------------------
// Parent side: the endpoint channel, its fault wrapper, gate-level resume.
// ---------------------------------------------------------------------------

/// Healthy parent-side endpoint channel (the CS0-A shape): carries the
/// successor's barrier demand to the still-live old Cell over the harness
/// temp files. Signer identity fields come from the child's registration
/// receipt, never from the response payload.
pub(crate) struct FileSignChannel<'a> {
    /// Spawned old Cell.
    pub child: &'a mut Child,
    /// Demand file path.
    pub request_path: PathBuf,
    /// Observation file path.
    pub response_path: PathBuf,
    /// Signer principal.
    pub issuer: PrincipalId,
    /// Signer control domain.
    pub control_domain_id: ControlDomainId,
    /// Signer key.
    pub key_id: KeyId,
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
            signature: BarrierObservationSignature {
                issuer: self.issuer,
                control_domain_id: self.control_domain_id,
                key_id: self.key_id,
                signature,
            },
        })
    }
}

/// The injected-channel fault wrapper (R7): composes around the healthy
/// [`FileSignChannel`] at the public [`BarrierObservationSource`] seam.
///
/// Two fault shapes, both leaving both processes alive:
/// - `cut` (persist): every demand fails closed without delivery — the
///   partition / dual-primary window;
/// - `fail_next` (one-shot): the next demand only — the mid-chain
///   transient delivery loss of the migration-fault variant.
pub(crate) struct CuttableSignChannel<'a> {
    /// The healthy channel being wrapped.
    pub inner: FileSignChannel<'a>,
    /// Persistent cut: while set, no demand is delivered.
    pub cut: bool,
    /// One-shot fault: the next single demand fails, then auto-clears.
    pub fail_next: bool,
}

impl<'a> CuttableSignChannel<'a> {
    /// Wraps the healthy channel, initially healthy.
    pub(crate) fn new(inner: FileSignChannel<'a>) -> Self {
        Self {
            inner,
            cut: false,
            fail_next: false,
        }
    }

    /// Opens the cut window.
    pub(crate) fn cut(&mut self) {
        self.cut = true;
    }

    /// Closes the cut window.
    pub(crate) fn restore(&mut self) {
        self.cut = false;
    }

    /// Arms the one-shot mid-chain fault.
    pub(crate) fn fail_next_barrier(&mut self) {
        self.fail_next = true;
    }

    /// Builder hook for call-scoped channel construction (the fault flags
    /// live with the caller when the channel is rebuilt per delivery).
    pub(crate) fn cut_if(&mut self, cut: bool) -> &mut Self {
        self.cut = cut;
        self
    }
}

impl BarrierObservationSource for CuttableSignChannel<'_> {
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        if self.cut {
            return Err(TakeoverDriveError::BarrierSource(
                "partition: the barrier observation channel is cut while both Cells stay alive"
                    .to_string(),
            ));
        }
        if self.fail_next {
            self.fail_next = false;
            return Err(TakeoverDriveError::BarrierSource(
                "mid-chain fault: one-shot barrier delivery loss".to_string(),
            ));
        }
        self.inner.observe_barrier(demand)
    }
}

// ---------------------------------------------------------------------------
// Durable raw-table evidence (independent SQLite reader, CS0-A convention).
// ---------------------------------------------------------------------------

/// Row count of one authority table through an independent connection.
pub(crate) fn raw_count(store_path: &Path, table: &str) -> i64 {
    let connection = rusqlite::Connection::open(store_path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

/// `(term, transition_code)` lease history rows in `lease_epoch` order.
pub(crate) fn lease_history_rows(store_path: &Path) -> Vec<(u64, i64)> {
    fn blob_u64(value: &[u8]) -> u64 {
        u64::from_be_bytes(value.try_into().expect("8-byte u64 blob"))
    }
    let connection = rusqlite::Connection::open(store_path).expect("open raw reader");
    let mut statement = connection
        .prepare(
            "SELECT term, transition_kind FROM task_authority_lease_history
             ORDER BY lease_epoch",
        )
        .expect("prepare lease history read");
    let rows = statement
        .query_map([], |row| {
            Ok((blob_u64(&row.get::<_, Vec<u8>>(0)?), row.get::<_, i64>(1)?))
        })
        .expect("read lease history rows");
    let mut out = Vec::new();
    for row in rows {
        out.push(row.expect("lease history row"));
    }
    out
}

/// `assignment_state` codes in insertion order (1 Active, 2
/// `TakeoverPending`, 3 Fenced).
pub(crate) fn assignment_state_codes(store_path: &Path) -> Vec<i64> {
    let connection = rusqlite::Connection::open(store_path).expect("open raw reader");
    let mut statement = connection
        .prepare("SELECT assignment_state FROM task_authority_assignments ORDER BY rowid")
        .expect("prepare assignment state read");
    let rows = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("read assignment states");
    let mut out = Vec::new();
    for row in rows {
        out.push(row.expect("assignment state row"));
    }
    out
}

/// Quarantine face of the no-preemption wait (R1.2.4): while the successor
/// waits for the incumbent lease to expire it must have driven *nothing* —
/// zero durable takeover rows, no assignment in `TakeoverPending` (the
/// fence prepare is the only writer of that state), and a term-1-only
/// lease history. The incumbent's own same-term registry rotations (each
/// lease-bound permit issuance may fence the prior Active assignment and
/// insert a new one) are its own legitimate history, not takeover effect,
/// so assignment codes are asserted takeover-free, not count-fixed.
pub(crate) fn assert_successor_wait_left_no_durable_effect(store_path: &Path) {
    assert_eq!(
        lease_history_rows(store_path),
        vec![(1, 1)],
        "no term advance while the incumbent lease is live"
    );
    let codes = assignment_state_codes(store_path);
    assert!(
        !codes.contains(&2),
        "no assignment may be TakeoverPending while waiting, got {codes:?}"
    );
    assert!(
        codes.last().is_some_and(|code| *code == 1),
        "the current assignment must be Active while waiting, got {codes:?}"
    );
    assert_eq!(
        raw_count(store_path, "task_authority_takeover_fence_receipts"),
        0,
        "no fence receipt while waiting"
    );
    assert_eq!(
        raw_count(store_path, "task_authority_takeover_receipts"),
        0,
        "no takeover receipt while waiting"
    );
    assert_eq!(
        raw_count(store_path, "task_authority_takeover_barrier_receipts"),
        0,
        "no barrier observation while waiting"
    );
}

// ---------------------------------------------------------------------------
// Gate-level resume of a pending takeover (the store's own idempotent
// gates, mirroring the drive's leg order and readbacks).
// ---------------------------------------------------------------------------

/// Durable facts of a takeover resumed through the store gates after a
/// mid-chain channel fault left the receipt pending.
pub(crate) struct GateCompletion {
    /// Pending receipt as read before resumption.
    pub pending: AuthorityTakeoverReceiptRecord,
    /// Stored signed observations, one per manifest member.
    pub observations: Vec<AuthorityTakeoverBarrierReceiptRecord>,
    /// Completion record.
    pub completion: AuthorityTakeoverCompletionRecord,
    /// Successor-registry reopen record.
    pub reopen: nlos_task::AuthoritySuccessorRegistryReopenRecord,
    /// Active assignment after the reopen.
    pub active_assignment: nlos_task::AuthorityAssignmentRecord,
    /// Registry record after the reopen.
    pub registry: nlos_task::ParticipantRegistryRecord,
}

/// Resumes one pending takeover through the store's own gates: reads the
/// frozen registry binding, resolves the fence receipt / pending receipt /
/// manifest, records one signed observation per member through `source`
/// (the restored channel), checks coverage, completes, reopens, and
/// re-reads the same final readbacks the drive asserts.
///
/// # Errors
///
/// A boxed store / federation failure, or a message describing any
/// readback that contradicts a gate's contract.
pub(crate) fn finish_pending_takeover<S: BarrierObservationSource>(
    store: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    source: &mut S,
    task_id: TaskId,
    lease: AuthorityLeaseRecord,
    now_ms: &mut dyn FnMut() -> i64,
) -> Result<GateCompletion, Box<dyn std::error::Error>> {
    let frozen = store.inspect_participant_registry(task_id)?;
    if frozen.state != nlos_task::ParticipantRegistryState::FrozenForTakeover {
        return Err("registry is not frozen for takeover".into());
    }
    let frozen_binding = nlos_task::ParticipantRegistryBinding {
        generation: frozen.generation,
        root: frozen.root,
    };
    let fence_receipt = store.inspect_authority_takeover_fence_receipt(task_id, frozen_binding)?;
    let exact_fence_set_root = fence_receipt
        .exact_fence_set_root
        .ok_or("frozen fence has no exact fence-set root")?;
    let pending = store.inspect_authority_takeover_receipt(task_id, fence_receipt.receipt_id)?;
    if pending.barrier_state != AuthorityTakeoverReceiptState::Pending
        || pending.new_assignment_id.is_some()
    {
        return Err("takeover receipt is not pending".into());
    }
    let members = store.inspect_authority_takeover_fence_members(task_id, frozen_binding)?;
    let mut observations = Vec::with_capacity(members.len());
    for member in &members {
        let now = now_ms();
        let observation = source.observe_barrier(nlos_slice_k::BarrierObservationDemand {
            takeover_receipt_id: pending.receipt_id,
            participant: member.participant,
            fence_set_root: exact_fence_set_root,
        })?;
        let record = store.record_authority_takeover_barrier_receipt_signed(
            identity,
            AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: pending.receipt_id,
                participant: member.participant,
                remote_receipt_id: observation.remote_receipt_id,
                barrier_digest: observation.barrier_digest,
                observed_at_ms: now,
            },
            observation.signature,
        )?;
        if record.signer.is_none() {
            return Err("signed barrier observation stored without a signer proof".into());
        }
        observations.push(record);
    }
    let coverage = store.inspect_authority_takeover_barrier_coverage(pending.receipt_id)?;
    if coverage.state != AuthorityTakeoverBarrierCoverageState::LocallyCovered {
        return Err(format!("coverage incomplete after resumption: {:?}", coverage.state).into());
    }
    let completion =
        store.complete_authority_takeover(nlos_task::CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: pending.receipt_id,
            lease,
            completed_at_ms: now_ms(),
        })?;
    let reopen =
        store.reopen_successor_registry(nlos_task::AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: pending.receipt_id,
            lease,
            reopened_at_ms: now_ms(),
        })?;
    let active_assignment = store.inspect_authority_assignment(task_id)?;
    if active_assignment.state != nlos_task::AuthorityAssignmentState::Active
        || active_assignment.assignment_id != reopen.active_assignment_id
        || active_assignment.participant_registry_binding != reopen.successor_registry_binding
        || active_assignment.authority_lease_binding != lease.binding()
    {
        return Err("active assignment does not bind the reopened registry and lease".into());
    }
    let registry = store.inspect_participant_registry(task_id)?;
    if registry.state != nlos_task::ParticipantRegistryState::Open
        || registry.generation != reopen.successor_registry_binding.generation
        || registry.root != reopen.successor_registry_binding.root
        || registry.prior_root != frozen_binding.root
    {
        return Err("registry is not the reopened successor generation".into());
    }
    Ok(GateCompletion {
        pending,
        observations,
        completion,
        reopen,
        active_assignment,
        registry,
    })
}

/// The CS0-C four-invariant assertion face, reused verbatim.
pub(crate) fn assert_invariants_all_hold(invariants: &nlos_slice_k::ConvergenceInvariants) {
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

/// Acquires the incumbent term-1 lease (the CS0-A child shape).
pub(crate) fn acquire_incumbent_lease(
    authority: &SqliteTaskAuthority,
    holder: ProcessId,
    idempotency_seed: u8,
    requested_at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRecord {
    match authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: holder,
            idempotency_key: IdempotencyKey::from_bytes([idempotency_seed; 16]),
            requested_at_ms,
            ttl_ms,
        })
        .expect("acquire the incumbent term-1 lease")
    {
        nlos_task::AuthorityLeaseDecision::Acquired(record) => record,
        other => panic!("expected a fresh acquire, got {other:?}"),
    }
}

/// Bootstraps the shared barrier signer principal (the CS0-A child shape)
/// and returns its identity binding.
pub(crate) fn bootstrap_barrier_signer(
    identity: &IdentityAuthority,
    seed: u8,
) -> nlos_identity::IdentityBinding {
    let signing_key = SigningKey::from_bytes(&[seed; 32]);
    identity
        .bootstrap_principal(nlos_identity::BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: nlos_identity::KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap the barrier signer")
        .binding()
}
