//! Cross-Cell takeover drive (CS0-A) — the `DIST-TASK-002` linearized CAS
//! chain plus the `DIST-TASK-004` successor-registry baseline, driven end
//! to end against one durable `nlos-task` authority by a successor.
//!
//! **What this module is.** A transport-neutral orchestrator over the
//! landed local gates of [`SqliteTaskAuthority`]:
//! `prepare_authority_takeover_fence` → one signed
//! `record_authority_takeover_barrier_receipt_signed` per exact-fence
//! manifest member → `complete_authority_takeover` →
//! `reopen_successor_registry`. The drive adds no store semantics of its
//! own: every durable transition is the store's own single-transaction
//! gate; this module only sequences them, fail-closes on readbacks that do
//! not match the gates' contracts, and returns the durable facts as one
//! outcome record for evidence assertions.
//!
//! **Transport shape (ADR-0019 R1.2.2 leaves the cross-Cell drive's
//! transport to the implementation; this is the Phase 0 choice, recorded
//! here per the C-SHARD plan).** The successor drives the chain by opening
//! the *same* durable task store as the old authority — same-host,
//! shared-filesystem SQLite multi-process access (ADR-0018 topology: one
//! OS process = one Cell; shared kernel, clock domain, and filesystem).
//! Barrier observations arrive through the [`BarrierObservationSource`]
//! seam: the endpoint host (the old Cell's process) mints its own remote
//! receipt identity and digest and signs the domain-separated observation
//! message; the successor submits them through the store's signed gate,
//! where the `nlos-identity` key authority verifies the Ed25519 proof
//! before anything becomes durable. That signed store gate is the same
//! code path the `nlos-takeover-control` SABI handler wraps, so this
//! shape keeps the verification semantics while leaving the production
//! transport decision (handler promotion or replacement) to Phase 1
//! CS1-D. No network protocol is invented and none is implied.
//!
//! **What this module is not.** It does not touch the Cell epoch: the
//! only epoch-advance entry stays the quota family's
//! `advance_epoch_and_quarantine` (pinned by the `cell_host` assembly);
//! this drive walks the `TaskAuthority` lease/term/fencing machinery
//! (schema v27+) exclusively. The successor registry is re-opened as a
//! *new* `Open` generation with a fresh root — the frozen old root is
//! never unfrozen in place (`DIST-TASK-004`). The store holds no clock by
//! repository convention, so the drive draws every timestamp from the
//! caller-supplied logical clock; the lease takeover uses the anchored
//! variant with the same reading as the wall observation, which in the
//! ADR-0018 shared-clock domain can only judge an incumbent lease more
//! alive, never more dead. Evidence produced through this drive is
//! single-host dual-process evidence: citing it as cross-host,
//! network-partition, or cross-clock evidence would violate RISK-B-11.

use std::error::Error;
use std::fmt;

use nlos_identity::IdentityAuthority;
use nlos_task::{
    AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeaseDecision,
    AuthorityLeaseRecord, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRecord,
    AuthorityLeaseTakeoverFenceRequest, AuthoritySuccessorRegistryReopenRecord,
    AuthoritySuccessorRegistryReopenRequest, AuthorityTakeoverBarrierCoverageState,
    AuthorityTakeoverBarrierReceiptRecord, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverCompletionRecord, AuthorityTakeoverFenceMemberRecord,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    CompleteAuthorityTakeoverRequest, ParticipantRecord, ParticipantRegistryBinding,
    ParticipantRegistryRecord, ParticipantRegistryState, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{IdempotencyKey, ProcessId, ReceiptId, TaskId};

/// What one fence-member endpoint is asked to attest: the pending
/// takeover receipt it is being barriered for, its own frozen participant
/// identity, and the exact fence-set root the observation must bind.
///
/// This is the successor's *demand*; the endpoint host answers with a
/// [`SignedBarrierObservation`]. The demand/response split is the seam
/// where the Phase 0 harness crosses the OS process boundary — the drive
/// itself never learns how the answer travelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierObservationDemand {
    /// Pending `TaskAuthorityTakeoverReceipt` the observation covers.
    pub takeover_receipt_id: ReceiptId,
    /// The endpoint's frozen participant record from the exact-fence
    /// manifest.
    pub participant: ParticipantRecord,
    /// Exact fence-set root of the frozen takeover fence.
    pub fence_set_root: [u8; 32],
}

/// One endpoint's answer to a [`BarrierObservationDemand`]: the endpoint's
/// own durable observation receipt identity and digest, plus the
/// principal Ed25519 signature over the domain-separated observation
/// message the store recomputes and verifies.
///
/// Every field is endpoint-authored material — the successor carries it
/// verbatim into the store's signed gate and the identity authority, not
/// the successor, decides whether it is trustworthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedBarrierObservation {
    /// Endpoint-durable receipt identity for this barrier observation.
    pub remote_receipt_id: ReceiptId,
    /// Endpoint-authored barrier digest.
    pub barrier_digest: [u8; 32],
    /// Principal Ed25519 signature over the observation message.
    pub signature: BarrierObservationSignature,
}

/// Source of signed barrier observations for one takeover drive — the
/// endpoint-facing half of the drive. Implemented by whatever carries the
/// successor's demand to the fence-member endpoints and back; the Phase 0
/// dual-process harness implements it over temp-file signaling to the old
/// Cell's process (see `tests/dual_cell_takeover.rs`).
///
/// # Errors
///
/// Implementations report their own channel failures through
/// [`TakeoverDriveError::BarrierSource`]; the drive fail-closes on the
/// first error and writes nothing further.
pub trait BarrierObservationSource {
    /// Obtains one endpoint's signed observation for the given demand.
    ///
    /// # Errors
    ///
    /// Returns a [`TakeoverDriveError::BarrierSource`] failure when the
    /// endpoint channel cannot deliver or the answer is unusable.
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError>;
}

/// Inputs of one cross-Cell takeover drive. The store and identity
/// handles are opened by the caller — the Phase 0 shape has the successor
/// open the old authority's task store (and the shared identity root)
/// directly off the shared filesystem; the drive is agnostic to how they
/// were obtained.
pub struct CrossCellTakeoverRequest<'a, S> {
    /// The durable task authority the chain is driven against. Phase 0:
    /// the old authority's store, opened by the successor process.
    pub store: &'a SqliteTaskAuthority,
    /// Identity authority that verifies barrier-observation signatures.
    /// Phase 0: the shared identity root both processes can read.
    pub identity: &'a IdentityAuthority,
    /// Endpoint channel used for the per-member barrier demands.
    pub barrier_source: &'a mut S,
    /// Task whose authority is being taken over.
    pub task_id: TaskId,
    /// Lease holder identity of the successor (its process, in the
    /// Phase 0 shape).
    pub successor_holder_id: ProcessId,
    /// Idempotency key of the successor lease request. Re-driving with
    /// the same key replays the same durable lease instead of minting a
    /// new term.
    pub lease_idempotency_key: IdempotencyKey,
    /// TTL of the successor lease. The drive's completion and reopen
    /// gates revalidate the lease against the clock, so the caller must
    /// keep the logical clock inside this window.
    pub lease_ttl_ms: i64,
    /// Caller-supplied logical clock (the store layer holds no clock by
    /// convention). Drawn once per gate in drive order; readings must be
    /// non-negative and monotone for the store's timestamp gates to
    /// accept them.
    pub now_ms: &'a mut dyn FnMut() -> i64,
}

/// Durable facts of one completed cross-Cell takeover drive: the
/// successor lease, the frozen fence with its exact roots and manifest,
/// the pending receipt as it stood right after the fence gate, every
/// stored signed barrier observation, the completion record, and the
/// successor-registry reopen with the final readbacks. Assertable field
/// by field as the schema v27–v38 durable evidence of the drive.
#[derive(Clone, Debug)]
pub struct CrossCellTakeoverOutcome {
    /// The successor term's lease record (term advanced from the old
    /// holder's).
    pub successor_lease: AuthorityLeaseRecord,
    /// Old assignment as read before the drive (the CAS chain's subject).
    pub old_assignment: AuthorityAssignmentRecord,
    /// Registry record as frozen by the fence gate
    /// (`FrozenForTakeover`).
    pub frozen_registry: ParticipantRegistryRecord,
    /// Immutable `FROZEN_FOR_TAKEOVER` fence receipt with the exact
    /// roots.
    pub fence_receipt: AuthorityLeaseTakeoverFenceRecord,
    /// Exact-fence member manifest (the barrier coverage set).
    pub fence_members: Vec<AuthorityTakeoverFenceMemberRecord>,
    /// Pending takeover receipt as read between the fence gate and
    /// completion (`Pending`, no successor assignment yet).
    pub takeover_receipt_pending: AuthorityTakeoverReceiptRecord,
    /// Stored signed barrier observations, one per manifest member.
    pub observations: Vec<AuthorityTakeoverBarrierReceiptRecord>,
    /// Completion record (`Pending → Complete`, successor assignment
    /// activated).
    pub completion: AuthorityTakeoverCompletionRecord,
    /// Successor-registry reopen record (new generation, new root, new
    /// active assignment).
    pub reopen: AuthoritySuccessorRegistryReopenRecord,
    /// Active assignment after the reopen, bound to the successor
    /// registry generation.
    pub active_assignment: AuthorityAssignmentRecord,
    /// Registry record after the reopen (new `Open` generation).
    pub reopened_registry: ParticipantRegistryRecord,
}

/// Fail-closed errors of the cross-Cell takeover drive. The store's own
/// gate failures surface verbatim as [`TakeoverDriveError::Store`]; the
/// remaining variants name drive-level preconditions and readbacks.
#[derive(Debug)]
pub enum TakeoverDriveError {
    /// A store gate refused the drive; carried verbatim.
    Store(TaskStoreError),
    /// The caller's logical clock produced a negative reading.
    NegativeClock,
    /// There is no authority lease yet to take over (`Acquired` shape) —
    /// a cross-Cell takeover needs an incumbent term.
    NoIncumbentAuthorityLease,
    /// The successor holder is the incumbent lease holder (`Renewed`
    /// shape) — not a cross-Cell takeover.
    SuccessorIsIncumbentHolder,
    /// The task has no current authority assignment to freeze.
    NoActiveAssignment,
    /// The current assignment is not `Active` (already takeover-pending
    /// or fenced).
    OldAssignmentNotActive {
        /// The assignment state the drive refused to continue from.
        state: AuthorityAssignmentState,
    },
    /// The frozen fence has no exact fence-set root, so the barrier
    /// coverage set cannot be resolved and the chain cannot proceed.
    FenceManifestUnavailable,
    /// The takeover receipt is already complete — the drive is one-shot
    /// per receipt; its durable facts are the evidence, not a re-drive.
    TakeoverAlreadyComplete,
    /// Barrier coverage is not locally complete before completion.
    BarrierCoverageIncomplete {
        /// The coverage state the drive refused to complete from.
        state: AuthorityTakeoverBarrierCoverageState,
        /// How many manifest members still lack a stored observation.
        missing: usize,
    },
    /// The endpoint channel could not deliver a signed observation.
    BarrierSource(String),
    /// A post-gate readback contradicted the gate's contract — treated
    /// as corruption of the drive's own expectations.
    PostCondition(&'static str),
}

impl fmt::Display for TakeoverDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "task authority gate: {error}"),
            Self::NegativeClock => {
                write!(
                    formatter,
                    "takeover drive clock produced a negative reading"
                )
            }
            Self::NoIncumbentAuthorityLease => write!(
                formatter,
                "no incumbent authority lease to take over (lease was acquired, not taken over)"
            ),
            Self::SuccessorIsIncumbentHolder => write!(
                formatter,
                "successor holder is the incumbent lease holder (lease was renewed, not taken over)"
            ),
            Self::NoActiveAssignment => {
                write!(formatter, "task has no current authority assignment")
            }
            Self::OldAssignmentNotActive { state } => write!(
                formatter,
                "current authority assignment is not active anymore: {state:?}"
            ),
            Self::FenceManifestUnavailable => write!(
                formatter,
                "frozen takeover fence has no exact fence-set root (manifest unavailable)"
            ),
            Self::TakeoverAlreadyComplete => write!(
                formatter,
                "takeover receipt is already complete; the drive is one-shot per receipt"
            ),
            Self::BarrierCoverageIncomplete { state, missing } => write!(
                formatter,
                "barrier coverage is not locally complete before completion: {state:?}, {missing} member(s) missing"
            ),
            Self::BarrierSource(reason) => {
                write!(formatter, "barrier observation channel: {reason}")
            }
            Self::PostCondition(reason) => {
                write!(
                    formatter,
                    "takeover drive post-condition violated: {reason}"
                )
            }
        }
    }
}

impl Error for TakeoverDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TaskStoreError> for TakeoverDriveError {
    fn from(error: TaskStoreError) -> Self {
        Self::Store(error)
    }
}

/// Draws the next logical timestamp, rejecting negative readings the
/// store's gates would refuse anyway.
fn next_now(now_ms: &mut dyn FnMut() -> i64) -> Result<i64, TakeoverDriveError> {
    let now = now_ms();
    if now < 0 {
        return Err(TakeoverDriveError::NegativeClock);
    }
    Ok(now)
}

/// Drives one complete cross-Cell takeover: successor lease (term
/// advance) → fence prepare → per-member signed barrier observations →
/// completion → successor-registry reopen, with fail-closed readbacks
/// after each leg. See the module documentation for the transport shape
/// and the honesty bound.
///
/// # Errors
///
/// Returns [`TakeoverDriveError::Store`] when any gate refuses (typed
/// lease, registry, coverage, or CAS failure — nothing is retried or
/// papered over), a drive-level precondition error before the first
/// mutation, or a post-condition error when a readback contradicts a
/// gate's contract.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)] // One function keeps the CAS-chain leg order visible.
pub fn drive_cross_cell_takeover<S: BarrierObservationSource>(
    request: CrossCellTakeoverRequest<'_, S>,
) -> Result<CrossCellTakeoverOutcome, TakeoverDriveError> {
    // Leg 0 — successor lease. The anchored judgement uses the same
    // logical reading as the wall observation: in the shared-clock ADR
    // topology that can only keep the incumbent more alive, never less.
    let lease_now_ms = next_now(request.now_ms)?;
    let wall_now_ms = u64::try_from(lease_now_ms).map_err(|_| TakeoverDriveError::NegativeClock)?;
    let successor_lease = match request.store.acquire_authority_lease_anchored(
        AuthorityLeaseRequest {
            holder_id: request.successor_holder_id,
            idempotency_key: request.lease_idempotency_key,
            requested_at_ms: lease_now_ms,
            ttl_ms: request.lease_ttl_ms,
        },
        wall_now_ms,
    )? {
        AuthorityLeaseDecision::TakenOver(record) | AuthorityLeaseDecision::Replayed(record) => {
            record
        }
        AuthorityLeaseDecision::Acquired(_) => {
            return Err(TakeoverDriveError::NoIncumbentAuthorityLease);
        }
        AuthorityLeaseDecision::Renewed(_) => {
            return Err(TakeoverDriveError::SuccessorIsIncumbentHolder);
        }
    };

    // The CAS chain's subject: the incumbent term's active assignment.
    let old_assignment = match request.store.inspect_authority_assignment(request.task_id) {
        Ok(record) => record,
        Err(TaskStoreError::ReceiptNotFound) => {
            return Err(TakeoverDriveError::NoActiveAssignment);
        }
        Err(error) => return Err(error.into()),
    };
    if old_assignment.state != AuthorityAssignmentState::Active {
        return Err(TakeoverDriveError::OldAssignmentNotActive {
            state: old_assignment.state,
        });
    }

    // Leg 1 — fence prepare: registry → `FrozenForTakeover`, exact roots,
    // manifest, old assignment → `TakeoverPending`, pending receipt.
    let fence_now_ms = next_now(request.now_ms)?;
    let frozen_registry =
        request
            .store
            .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
                task_id: request.task_id,
                expected_registry_binding: old_assignment.participant_registry_binding,
                lease: successor_lease,
                requested_at_ms: fence_now_ms,
            })?;
    if frozen_registry.state != ParticipantRegistryState::FrozenForTakeover {
        return Err(TakeoverDriveError::PostCondition(
            "registry is not frozen for takeover after the fence gate",
        ));
    }
    let frozen_binding = ParticipantRegistryBinding {
        generation: frozen_registry.generation,
        root: frozen_registry.root,
    };
    if frozen_binding != old_assignment.participant_registry_binding {
        return Err(TakeoverDriveError::PostCondition(
            "frozen registry binding drifted from the old assignment binding",
        ));
    }
    let fence_receipt = request
        .store
        .inspect_authority_takeover_fence_receipt(request.task_id, frozen_binding)?;
    let exact_fence_set_root = fence_receipt
        .exact_fence_set_root
        .ok_or(TakeoverDriveError::FenceManifestUnavailable)?;
    let takeover_receipt_pending = request
        .store
        .inspect_authority_takeover_receipt(request.task_id, fence_receipt.receipt_id)?;
    if takeover_receipt_pending.barrier_state != AuthorityTakeoverReceiptState::Pending
        || takeover_receipt_pending.new_assignment_id.is_some()
    {
        return Err(TakeoverDriveError::TakeoverAlreadyComplete);
    }
    let fence_members = request
        .store
        .inspect_authority_takeover_fence_members(request.task_id, frozen_binding)?;

    // Leg 2 — one signed barrier observation per exact-fence member. The
    // endpoint hosts sign; the store's gate verifies through the identity
    // authority before anything becomes durable.
    let mut observations = Vec::with_capacity(fence_members.len());
    for member in &fence_members {
        let observed_at_ms = next_now(request.now_ms)?;
        let observation = request
            .barrier_source
            .observe_barrier(BarrierObservationDemand {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                participant: member.participant,
                fence_set_root: exact_fence_set_root,
            })?;
        let record = request
            .store
            .record_authority_takeover_barrier_receipt_signed(
                request.identity,
                AuthorityTakeoverBarrierReceiptRequest {
                    takeover_receipt_id: takeover_receipt_pending.receipt_id,
                    participant: member.participant,
                    remote_receipt_id: observation.remote_receipt_id,
                    barrier_digest: observation.barrier_digest,
                    observed_at_ms,
                },
                observation.signature,
            )?;
        if record.signer.is_none() {
            return Err(TakeoverDriveError::PostCondition(
                "signed barrier observation stored without a signer proof",
            ));
        }
        observations.push(record);
    }

    // Pre-completion coverage gate: every manifest member observed at the
    // same exact root — the read-only view the store recomputes.
    let coverage = request
        .store
        .inspect_authority_takeover_barrier_coverage(takeover_receipt_pending.receipt_id)?;
    if coverage.state != AuthorityTakeoverBarrierCoverageState::LocallyCovered {
        return Err(TakeoverDriveError::BarrierCoverageIncomplete {
            state: coverage.state,
            missing: coverage.missing_participants.len(),
        });
    }

    // Leg 3 — completion: receipt `Pending → Complete`, old assignment
    // fenced, successor assignment activated on the frozen binding.
    let completed_at_ms = next_now(request.now_ms)?;
    let completion =
        request
            .store
            .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                lease: successor_lease,
                completed_at_ms,
            })?;
    if completion.barrier_state != AuthorityTakeoverReceiptState::Complete {
        return Err(TakeoverDriveError::PostCondition(
            "completion gate returned without completing the receipt",
        ));
    }

    // Leg 4 — successor registry reopen (`DIST-TASK-004`): a new `Open`
    // generation with a fresh root; the frozen old root is never unfrozen
    // in place.
    let reopened_at_ms = next_now(request.now_ms)?;
    let reopen =
        request
            .store
            .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                lease: successor_lease,
                reopened_at_ms,
            })?;
    if reopen.successor_registry_binding.generation <= frozen_binding.generation
        || reopen.successor_registry_binding.root == frozen_binding.root
    {
        return Err(TakeoverDriveError::PostCondition(
            "successor registry reopen did not advance to a new generation and root",
        ));
    }

    // Final readbacks — the durable state the evidence asserts against.
    let active_assignment = request
        .store
        .inspect_authority_assignment(request.task_id)?;
    if active_assignment.state != AuthorityAssignmentState::Active
        || active_assignment.assignment_id != reopen.active_assignment_id
        || active_assignment.participant_registry_binding != reopen.successor_registry_binding
        || active_assignment.authority_lease_binding != successor_lease.binding()
    {
        return Err(TakeoverDriveError::PostCondition(
            "active assignment does not bind the reopened successor registry and lease",
        ));
    }
    let reopened_registry = request
        .store
        .inspect_participant_registry(request.task_id)?;
    if reopened_registry.state != ParticipantRegistryState::Open
        || reopened_registry.generation != reopen.successor_registry_binding.generation
        || reopened_registry.root != reopen.successor_registry_binding.root
        || reopened_registry.prior_root != frozen_binding.root
    {
        return Err(TakeoverDriveError::PostCondition(
            "registry is not the reopened successor generation chained to the frozen root",
        ));
    }

    Ok(CrossCellTakeoverOutcome {
        successor_lease,
        old_assignment,
        frozen_registry,
        fence_receipt,
        fence_members,
        takeover_receipt_pending,
        observations,
        completion,
        reopen,
        active_assignment,
        reopened_registry,
    })
}
