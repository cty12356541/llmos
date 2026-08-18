//! Durable single-authority Task store for NLOS (B-TASK slices 001–006).
//!
//! This crate implements the durable `TaskAuthority` subset required by the
//! B-TASK acceptance gates: Task registration, frozen-input snapshot digest
//! binding, `TaskHead` revision CAS, dual `TaskAttempt` registration with
//! independent generations and cancellation scopes, unique `CommitPermit`
//! issuance (`[TASK-COMMIT-001]`), cancel/permit linearization on one
//! control/cancel/permit epoch (`[TASK-CANCEL-002]` / `[TASK-CANCEL-003]`),
//! crash/restart recovery with no ghost permits, per-slot `EffectPermit`
//! issuance with one-shot dispatch tokens and the
//! `PLANNED → PERMITTED → DISPATCHED → EFFECT_CLOSED|EFFECT_UNKNOWN` /
//! `NO_EFFECT` slot state machine (`[TASK-EFFECT-001]` /
//! `[TASK-EFFECT-002]`), and — since schema v3 — the single-authority
//! `EFFECT_UNKNOWN` quarantine/adoption/reconcile lifecycle
//! (`[TASK-EFFECT-003]` / `[TASK-COMMIT-003]`), the durable cross-attempt
//! `TaskEffectHistoryEntry` table with `TaskEffectHistoryRoot` and
//! retry-fence advancement (`[TASK-EFFECT-ID-001]` /
//! `[TASK-RETRY-EFFECT-001]`), and the full required-slot success
//! semantics of `[TASK-COMMIT-002]`. Since schema v4 the crate also
//! carries the single-authority `TaskGroup` organization layer:
//! acyclic parent/child trees with `max_depth`/`max_children`
//! enforcement, content-addressed membership with monotonic generation
//! CAS and immutable Admission/Removal receipts (`[TASK-GROUP-002]`),
//! optional group binding for attempts (bit-exact membership
//! generation/root/policy drift fence), structural tree cancellation
//! (`[TASK-CANCEL-001]` / `[TASK-CANCEL-002]`), and the derived ALL/ANY
//! aggregate state with the quarantine cap (`[TASK-STATE-002]` subset,
//! `[TASK-GROUP-002]` final clause). Schema v5 additionally snapshots the
//! current membership generation/root/policy with each grouped write set /
//! `CommitPermit`, revalidates it before effect dispatch and terminalization,
//! and copies it verbatim into `TaskCommitReceipt`-shaped records.
//! Schema v6 begins the recoverable cross-authority commit path by binding
//! an immutable, canonical Artifact publication plan to an issued permit's
//! `write_set_root`; planning itself never authorizes publication or advances
//! `TaskHead`. Sealed proposed Artifact writes may now feed that plan, while
//! effectful terminal Task finalization remains guarded separately.
//! Schema v8 adds a per-plan durable recovery ledger: retry due time,
//! deterministic jitter, consecutive/total failure counts, and escalation
//! survive `TaskAuthority` process restart and resolve with terminal commit.
//! Schema v18 adds immutable owner-read endpoint proofs for planned effect
//! slots and folds their participant binding into the canonical write-set
//! root; schema v19 adds authority-checked proposed Artifact writes, and
//! schema v20 separates the permit-bound `TaskWriteSet` root from the
//! staging-bearing Artifact publication-plan root. Schema v21 adds owner-
//! verified Semantic append declarations, target-scope matching, and direct
//! durable `AdmissionReceipt` identity binding. Schema v22 additionally
//! carries an optional owner-verified `DurabilityReceipt` identity, and
//! schema v23 carries an optional caller-declared admission-policy digest,
//! without inferring publication from an outbox row. Schema v24 adds the
//! owner-verified Operation endpoint to the per-effect endpoint and
//! participant registries. These remain local partial proofs, not
//! cross-authority activation or complete publication. Schema v25 adds
//! Task-side Semantic publication plans and immutable nested owner receipt
//! copies; Semantic-only plans can finalize with a nested
//! `SemanticTaskCommitReceipt`, while mixed Effect + Semantic commits remain
//! on the unified coordinator path.
//! The Semantic-aware v3 finalization entry point re-reads those owner proofs
//! for an issued permit before the Task CAS; replayed terminal permits keep
//! the normal idempotent path and no publication receipt is synthesized.
//!
//! Explicitly out of scope: complete cross-authority lease authentication,
//! takeover adoption, and fault handling. Schema v27 provides only a durable local
//! `TaskAuthority` lease/term/fencing primitive; schema v28 adds an opt-in
//! immutable `CommitPermit` binding and plain v3/pre-effect terminal guards;
//! schema v29 adds a same-term lease-bound adoption/reconcile guard and an
//! opt-in local `FROZEN_FOR_TAKEOVER` pre-gate; schema v30 persists an
//! immutable local fence receipt whose local exact-fence and outstanding-set
//! roots are computed when the durable participant mapping is complete,
//! without remote barrier receipts. No schema authorizes an IPC peer or
//! completes cross-term adoption; schema v31 adds an immutable local
//! `TaskAuthorityAssignment` baseline for lease-bound permits, but no successor
//! assignment activation; schema v32 persists only a pending local
//! `TaskAuthorityTakeoverReceipt` prefix linked to the old assignment and
//! frozen fence, without remote barrier receipts or successor activation.
//! Schema v33 persists immutable per-endpoint barrier observations bound to
//! that pending prefix and exact local fence-set root, but does not verify
//! remote attestation or advance the takeover state.
//! Schema v34 persists the canonical exact-fence member manifest so barrier
//! observations can be matched against the full local fence set, including
//! durable outstanding-operation participants.
//! Schema v35 persists the endpoint-supplied barrier digest for new
//! observations; legacy v33/v34 observations retain an unknown (`NULL`)
//! digest rather than fabricating one during migration.
//! Compensation execution
//! (`COMPENSATED` is recordable but never executed), `QUORUM`/`REDUCE`
//! group semantics (`[TASK-GROUP-003]`), `BEST_EFFORT` failure mode,
//! `AGENT_INSTANCE` members, `DETACH` execution (`[TASK-DETACH-001]`),
//! ResourceGroup/ResourceAccount enforcement (placeholder binding
//! fields), Namespace delegation, TaskPlan/TaskNode materialization,
//! full Process BirthDecision/host enforcement, full `IsolationDomain`
//! lifecycle, operation prepare→activate, Channel endpoints, signatures,
//! and any IPC surface. Artifact publication
//! authorization is now a durable `TaskAuthority` fence; READY-only
//! Artifact-aware Task finalize now links nested receipts
//! atomically inside `TaskAuthority`. `ArtifactAuthority` online verification
//! remains outside schema v8; the automatic cross-authority driver and worker
//! live in `nlos-commit-coordinator` to avoid a reverse crate dependency. Post-permit
//! `EFFECTING`/`FINALIZING`/`UNCERTAIN`/`RECONCILING` from the §25.1
//! attempt state machine are represented as permit/slot states rather
//! than attempt states here.

mod commit;
mod effect;
mod group;
mod lease;
mod migrations;
mod model;
mod participant;
mod reconcile;
mod recovery;
mod semantic_commit;
mod store;

pub use commit::{
    ArtifactCommitPlanDecision, ArtifactCommitPlanId, ArtifactCommitPlanRecord,
    ArtifactCommitPlanState, ArtifactCommitProgress, ArtifactFinalizeDecision,
    ArtifactPublicationAuthorizationDecision, ArtifactPublicationExpectation,
    ArtifactTaskCommitReceipt, FinalizeArtifactCommitRequest, NestedArtifactPublicationReceipt,
    PlanArtifactCommitRequest, RecordArtifactPublicationsRequest, artifact_publication_plan_root,
};
pub use effect::{
    DispatchRequest, EffectPermitDecision, EffectReceipt, EffectReceiptDecision, IssuedPermit,
    LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitRequest as EffectPermitRequest, ReceiptKind, SetSummary, SlotRecord, SlotState,
    empty_effect_set_root, expected_success_assertion_digest, idempotency_identity_digest,
};
pub use group::{
    AttemptGroupBindingRecord, AttemptGroupRegistration, CompletionMode, FailureMode,
    GroupAdmissionReceiptRecord, GroupBinding, GroupCancelDecision, GroupCancelRequest,
    GroupMemberRecord, GroupMemberRef, GroupMemberType, GroupReceiptKind, GroupRecord,
    GroupRegistrationDecision, GroupSpec, GroupState, MembershipState, RemovalDecision,
    RemoveMemberRequest, TaskGroupCommitBinding, empty_group_membership_root, membership_root_of,
};
pub use lease::{
    AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeaseBinding,
    AuthorityLeaseDecision, AuthorityLeasePermitRequest, AuthorityLeaseRecord,
    AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRecord, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverage, AuthorityTakeoverBarrierCoverageState,
    AuthorityTakeoverBarrierReceiptRecord, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverBarrierReceiptState, AuthorityTakeoverFenceMemberRecord,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, MAX_AUTHORITY_LEASE_TTL_MS,
};
pub use model::{
    AdoptionReceiptRecord, AttemptHandle, AttemptRecord, AttemptRegistrationDecision, AttemptSpec,
    AttemptState, CancelDecision, CancelRequest, ClosePermitDecision, ClosePermitRequest,
    ClosedAttempt, EffectHistoryEntry, EffectHistoryLookup, EffectHistoryOutcome, FinalizeDecision,
    FinalizeRequest, PermitClosureOutcome, PermitConflict, PermitDecision, PermitRecord,
    PermitRequest, PermitState, PlannedEffect, QuarantineReceiptRecord, ReceiptOutcome,
    ReconcileOutcome, ReconciliationReceiptRecord, RequiredSatisfaction, RequiredSatisfactionProof,
    SnapshotBundle, SnapshotConsistency, TaskReceiptRecord, TaskRecord, TaskRegistrationDecision,
    TaskSnapshotReceiptRecord, TaskSnapshotReceiptSpec, TaskSpec, TaskState,
    TaskWriteSetArtifactRead, TaskWriteSetArtifactWrite, TaskWriteSetArtifactWriteRequest,
    TaskWriteSetDecision, TaskWriteSetEffectEndpoint, TaskWriteSetEffectEndpointKind,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetProcessBinding,
    TaskWriteSetProcessBindingRequest, TaskWriteSetRecord, TaskWriteSetRequest,
    TaskWriteSetResourceReservation, TaskWriteSetResourceReservationRequest,
    TaskWriteSetSemanticAppend, TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRead,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
pub use nlos_types::{EffectPermitId, EffectSlotId, TaskGroupId};
pub use participant::{
    ParticipantRecord, ParticipantRegistrationDecision, ParticipantRegistryBinding,
    ParticipantRegistryRecord, ParticipantRegistryState, ParticipantType,
};
pub use reconcile::{
    AdoptionReplay, AdoptionRequest, AuthorityLeaseAdoptionRequest, AuthorityLeaseCloseRequest,
    AuthorityLeaseFinalizeRequest, AuthorityLeaseReconcileRequest, FinalizeRequestV3,
    ReconcileReplay, ReconcileRequest, effect_history_root_of,
};
pub use recovery::{
    ArtifactRecoveryAlert, ArtifactRecoveryAlertAcknowledgeDecision,
    ArtifactRecoveryAlertAcknowledgeRequest, ArtifactRecoveryAlertReceipt,
    ArtifactRecoveryFailureRequest, ArtifactRecoveryFailureSource, ArtifactRecoveryRecord,
    ArtifactRecoveryResumeRequest, ArtifactRecoveryState, ArtifactRecoverySummary,
};
pub use semantic_commit::{
    FinalizeSemanticCommitRequest, NestedSemanticPublicationReceipt, PlanSemanticCommitRequest,
    PrepareSemanticFinalizeRequest, RecordSemanticPublicationsRequest, SemanticCommitPlanDecision,
    SemanticCommitPlanId, SemanticCommitPlanRecord, SemanticCommitPlanState,
    SemanticCommitProgress, SemanticFinalizeDecision, SemanticFinalizeEnvelopeDecision,
    SemanticFinalizeEnvelopeRecord, SemanticPublicationAuthorizationDecision,
    SemanticTaskCommitReceipt,
};
pub use store::SqliteTaskAuthority;

use std::error::Error;
use std::fmt;

/// Errors produced by the durable task authority.
///
/// Storage-level failures, durability negotiation, and schema validation
/// mirror `nlos-store`; domain violations (stale handles, holder mismatch,
/// invalid transitions) are typed so callers can distinguish safely
/// retryable conditions (`[TASK-CONFLICT-001]`).
#[derive(Debug)]
pub enum TaskStoreError {
    Sqlite(rusqlite::Error),
    CorruptRecord(&'static str),
    UnsupportedSchema(i64),
    LockPoisoned,
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// No current authority lease exists for the `TaskAuthority`.
    AuthorityLeaseNotFound,
    /// A different holder attempted to acquire an unexpired authority lease.
    AuthorityLeaseHeld,
    /// A lease was presented after its durable expiry.
    AuthorityLeaseExpired,
    /// A lease term, epoch, holder, or fencing token is stale.
    AuthorityLeaseFenced,
    /// A lease request violates its timestamp, TTL, or idempotency contract.
    InvalidAuthorityLease {
        /// Static explanation of the rejected lease invariant.
        reason: &'static str,
    },
    /// A permit or terminal mutation is bound to a lease but no lease was
    /// supplied by the caller.
    AuthorityLeaseRequired,
    /// A supplied lease does not match the immutable permit binding.
    AuthorityLeaseBindingMismatch,
    /// No task with the given ID is registered.
    TaskNotFound,
    /// No attempt with the given ID exists under the given task.
    AttemptNotFound,
    /// No permit with the given ID exists under the given task.
    PermitNotFound,
    /// No receipt with the given ID exists under the given task.
    ReceiptNotFound,
    /// No durable snapshot receipt with the given ID exists under the task.
    SnapshotReceiptNotFound,
    /// No Artifact publication plan with this identity exists.
    ArtifactCommitPlanNotFound,
    /// No Semantic publication plan with this identity exists.
    SemanticCommitPlanNotFound,
    /// The Artifact publication plan is not complete enough to finalize.
    ArtifactCommitPlanNotReady {
        state: ArtifactCommitPlanState,
    },
    /// The Semantic publication plan is not complete enough to finalize.
    SemanticCommitPlanNotReady {
        state: SemanticCommitPlanState,
    },
    /// Recovery retry timing, threshold, or timestamp is invalid.
    InvalidArtifactRecoveryPolicy {
        reason: &'static str,
    },
    /// An escalated recovery resume used a stale failure-count CAS.
    ArtifactRecoveryCasMismatch {
        expected: u64,
        current: u64,
    },
    /// No retry/escalation ledger exists for the plan.
    ArtifactRecoveryNotFound,
    /// The requested recovery transition is invalid for its durable state.
    InvalidArtifactRecoveryState {
        state: ArtifactRecoveryState,
    },
    /// The task ID is already registered with a different specification.
    DuplicateTask,
    /// The attempt ID is already registered with a different specification.
    DuplicateAttempt,
    /// A snapshot ID was rebound to different digest bytes; snapshots are
    /// immutable once inserted (`[TASK-SNAPSHOT-001]`).
    SnapshotConflict,
    /// The snapshot receipt is stale, structurally incomplete, or conflicts
    /// with an immutable receipt/snapshot binding.
    InvalidSnapshotReceipt {
        /// Static explanation of the rejected invariant.
        reason: &'static str,
    },
    /// An idempotency key was replayed with different request bytes.
    IdempotencyConflict,
    /// The Artifact publication expectation set is empty, ambiguous, or
    /// does not equal the sealed declaration or permit-bound write-set root.
    InvalidArtifactPublicationPlan {
        /// Static explanation of the rejected invariant.
        reason: &'static str,
    },
    /// An Artifact publication receipt conflicts with the immutable plan
    /// or an already consumed receipt.
    ArtifactPublicationConflict {
        /// Static explanation of the rejected binding.
        reason: &'static str,
    },
    /// A Semantic publication plan or nested owner receipt conflicts with
    /// the immutable `TaskWriteSet` binding.
    InvalidSemanticPublicationPlan {
        /// Static explanation of the rejected binding.
        reason: &'static str,
    },
    /// A Semantic publication receipt conflicts with an already consumed
    /// receipt or immutable plan row.
    SemanticPublicationConflict {
        /// Static explanation of the rejected binding.
        reason: &'static str,
    },
    /// Membership mutation is frozen while a group-bound Artifact plan is
    /// authorized but not yet atomically finalized.
    GroupPublicationInFlight,
    /// The caller presented a stale generation for an existing object.
    InvalidGeneration,
    /// The attempt is not in a state that allows the requested transition.
    InvalidAttemptState {
        state: AttemptState,
    },
    /// Attempts cannot be registered on a cancelled task.
    TaskCancelled,
    /// The finalize caller does not match the permit's attempt binding.
    NotPermitHolder,
    /// The permit is not in `Issued` state and the request is not an exact
    /// replay of the original finalize.
    PermitNotIssued,
    /// The current `TaskHead` no longer matches the permit's expected head.
    StaleTaskHead,
    /// A finalize tried to move the retry-fence epoch backwards.
    FenceRegression,
    /// A monotonic epoch/sequence space is exhausted; fail closed instead
    /// of wrapping.
    EpochExhausted,
    /// The declared planned effect set violates `[TASK-EFFECT-002]`
    /// (descriptor not bound to this task/generation, or a duplicate
    /// `LogicalEffectId` inside one set).
    InvalidEffectSet {
        /// Static explanation of the violation.
        reason: &'static str,
    },
    /// No slot with the given `effect_seq` was declared for the permit.
    EffectSlotNotFound,
    /// No `EffectPermit` with the given ID exists under the given task.
    EffectPermitNotFound,
    /// The caller presented a permit epoch different from the outstanding
    /// permit's epoch.
    PermitEpochMismatch,
    /// A cancellation committed after the (effect) permit was issued; the
    /// pre-cancel window stays untouched for the cancel path to close
    /// (`[TASK-CANCEL-003]`).
    CancellationCommitted {
        /// The cancel epoch now durable on the task.
        cancel_epoch: u64,
    },
    /// The effect slot is not in a state that allows the requested
    /// transition (`[TASK-EFFECT-002]`).
    InvalidEffectSlotState {
        /// The slot's current durable state.
        state: SlotState,
    },
    /// The presented dispatch token does not match the issued one-shot
    /// token.
    DispatchTokenMismatch,
    /// The presented dispatch token was already consumed; re-dispatch is
    /// refused fail-closed (`[TASK-EFFECT-001]`).
    DispatchTokenConsumed,
    /// Finalize is blocked by declared slots that have not reached a known
    /// terminal state (`[TASK-COMMIT-002]` subset).
    OutstandingEffectSlots {
        /// How many declared slots still block closure.
        count: u64,
    },
    /// `ConditionNotApplicable` was requested for a slot without a
    /// pre-bound `required_condition_digest`.
    ConditionNotBound,
    /// The active permit is a non-reusable quarantine tombstone
    /// (`[TASK-EFFECT-003]`): the requested mutation is blocked until
    /// every unknown slot is reconciled.
    Quarantined,
    /// A `PermitAdoptionReceipt` scope is
    /// `RECONCILE_CLOSE_OR_QUARANTINE_ONLY` (`[TASK-COMMIT-003]`): it never
    /// authorizes new `EffectPermit`s, dispatches, effects, or proposal
    /// changes.
    AdoptionScopeViolation,
    /// The declared logical effect is already `EFFECT_CLOSED` in the
    /// durable effect history; silent re-dispatch is refused fail-closed
    /// (`[TASK-RETRY-EFFECT-001]`).
    EffectAlreadyClosed,
    /// A required slot cannot satisfy `COMMITTED` with the presented proof
    /// (`[TASK-COMMIT-002]`).
    RequiredEffectUnsatisfied {
        /// The unsatisfied required slot.
        effect_seq: u64,
        /// Static explanation of the violation.
        reason: &'static str,
    },
    /// The slot or permit is not in the durable state the reconcile API
    /// requires (`[TASK-EFFECT-003]`).
    InvalidReconcileState {
        /// Static explanation of the violation.
        reason: &'static str,
    },
    /// The presented finalize/finalize-replay bytes conflict with the
    /// durable lifecycle record.
    HistoryConflict,
    /// A `FAILED_BEFORE_EFFECT`/`CANCELLED_BEFORE_EFFECT` closure was
    /// requested although at least one effect provably happened
    /// (`[TASK-RETRY-EFFECT-001]` forbids that path).
    PermitHasEffects {
        /// How many slots closed with an effect.
        count: u64,
    },
    /// No group with the given ID exists (for the given task).
    GroupNotFound,
    /// The group ID is already registered with a different specification.
    DuplicateGroup,
    /// The requested parent binding would create a parent/child cycle
    /// (`[TASK-GROUP-001]`).
    GroupCycle,
    /// The child would exceed an ancestor group's `max_depth` bound
    /// (`[TASK-GROUP-001]`).
    GroupDepthExceeded,
    /// The group's `max_children` bound is exhausted
    /// (`[TASK-GROUP-001]`).
    GroupFanoutExceeded,
    /// The group is SEALED; no new child is admitted
    /// (`[TASK-GROUP-002]`).
    GroupSealed,
    /// The group is not OPEN for the requested membership mutation.
    GroupNotOpen {
        /// The group's current durable state.
        state: GroupState,
    },
    /// The group is in a state that rejects the requested transition.
    InvalidGroupState {
        /// The group's current durable state.
        state: GroupState,
    },
    /// The presented membership root / policy digest / member identity
    /// differs from the durable record (fail-closed,
    /// `[TASK-GROUP-002]`).
    MembershipConflict,
    /// The caller presented a membership generation different from the
    /// group's current generation (`[TASK-GROUP-002]`).
    StaleMembershipGeneration {
        /// The generation the caller presented.
        expected: u64,
        /// The group's current durable generation.
        current: u64,
    },
    /// The member is not present in the group.
    GroupMemberNotFound,
    /// A reserved group mode or member type was requested
    /// (`QUORUM`/`REDUCE`/`BEST_EFFORT`/`AGENT_INSTANCE` are not
    /// producible in this slice).
    UnsupportedGroupMode,
    /// Removal of a member carrying quarantine evidence would launder
    /// the `[TASK-GROUP-002]` final-clause cap.
    GroupQuarantinedChild,
    /// No participant registry exists for the task.
    ParticipantRegistryNotFound,
    /// The registry is frozen and rejects the requested transition.
    ParticipantRegistryFrozen {
        state: ParticipantRegistryState,
    },
    /// The registry state changed outside the expected authority CAS.
    ParticipantRegistryCasMismatch,
    /// A legacy permit or receipt lacks the registry binding required for
    /// a new effect-plane or terminal authority decision.
    ParticipantRegistryBindingMissing,
    /// A copied effect/receipt binding differs from its parent permit or
    /// from the frozen authority registry.
    ParticipantRegistryBindingMismatch,
    /// A verified endpoint identity or Receipt collides with another tuple.
    ParticipantEndpointConflict,
    /// The bounded participant set is full.
    ParticipantRegistryFull,
    /// A `TaskWriteSet` identity was reused with different sealed bytes.
    TaskWriteSetConflict {
        /// Static explanation of the conflicting binding.
        reason: &'static str,
    },
    /// The requested Artifact read revision/digest differs from owner
    /// authority readback.
    TaskWriteSetReadConflict,
    /// The requested Semantic event log sequence or canonical bytes differ
    /// from `SemanticAuthority` readback.
    TaskWriteSetSemanticReadConflict,
    /// A requested Reservation is not the expected current RESERVED binding.
    TaskWriteSetResourceReservationConflict,
    /// A `TaskWriteSet` is required for the verified permit path but missing.
    TaskWriteSetNotFound,
    /// Artifact authority proof readback failed before Task mutation.
    ArtifactParticipantAuthority(nlos_artifact::ArtifactError),
    /// Semantic authority proof readback failed before Task mutation.
    SemanticParticipantAuthority(nlos_semantic::SemanticAuthorityError),
    /// Resource authority proof readback failed before Task mutation.
    ResourceParticipantAuthority(nlos_resource::ResourceAuthorityError),
    /// Process authority proof readback failed before Task mutation.
    ProcessParticipantAuthority(nlos_process::ProcessAuthorityError),
    /// Operation authority proof readback failed before Task mutation.
    OperationParticipantAuthority(nlos_store::StoreError),
    /// The owner proof does not match the generation planned by the caller.
    ParticipantEndpointGenerationMismatch {
        expected: u64,
        current: u64,
    },
    /// A caller-supplied group specification violates a structural bound.
    InvalidGroupSpec {
        /// Static explanation of the violation.
        reason: &'static str,
    },
}

// A flat Display match grows linearly with the variant count; splitting
// it by category would only obscure the one-message-per-variant table.
#[allow(clippy::too_many_lines)]
impl fmt::Display for TaskStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite authority failure: {error}"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported task schema version {version}")
            }
            Self::LockPoisoned => formatter.write_str("authority writer lock is poisoned"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::AuthorityLeaseNotFound => {
                formatter.write_str("TaskAuthority has no durable authority lease")
            }
            Self::AuthorityLeaseHeld => {
                formatter.write_str("authority lease is held by another live term")
            }
            Self::AuthorityLeaseExpired => formatter.write_str("authority lease has expired"),
            Self::AuthorityLeaseFenced => {
                formatter.write_str("authority lease term or fencing token is stale")
            }
            Self::InvalidAuthorityLease { reason } => {
                write!(formatter, "invalid authority lease: {reason}")
            }
            Self::AuthorityLeaseRequired => {
                formatter.write_str("this permit mutation requires its authority lease")
            }
            Self::AuthorityLeaseBindingMismatch => {
                formatter.write_str("authority lease does not match the permit binding")
            }
            Self::TaskNotFound => formatter.write_str("task is not registered"),
            Self::AttemptNotFound => formatter.write_str("attempt does not exist under the task"),
            Self::PermitNotFound => formatter.write_str("permit does not exist under the task"),
            Self::ReceiptNotFound => formatter.write_str("receipt does not exist under the task"),
            Self::SnapshotReceiptNotFound => {
                formatter.write_str("snapshot receipt does not exist under the task")
            }
            Self::ArtifactCommitPlanNotFound => {
                formatter.write_str("artifact commit plan does not exist")
            }
            Self::SemanticCommitPlanNotFound => {
                formatter.write_str("semantic commit plan does not exist")
            }
            Self::ArtifactCommitPlanNotReady { state } => {
                write!(
                    formatter,
                    "artifact commit plan state {state:?} is not ready"
                )
            }
            Self::SemanticCommitPlanNotReady { state } => {
                write!(
                    formatter,
                    "semantic commit plan state {state:?} is not ready"
                )
            }
            Self::InvalidArtifactRecoveryPolicy { reason } => {
                write!(formatter, "invalid Artifact recovery policy: {reason}")
            }
            Self::ArtifactRecoveryCasMismatch { expected, current } => write!(
                formatter,
                "Artifact recovery CAS expected {expected} failures but found {current}"
            ),
            Self::ArtifactRecoveryNotFound => {
                formatter.write_str("Artifact recovery ledger does not exist")
            }
            Self::InvalidArtifactRecoveryState { state } => {
                write!(
                    formatter,
                    "Artifact recovery state {state:?} rejects the transition"
                )
            }
            Self::DuplicateTask => formatter.write_str("task ID re-registered with new spec"),
            Self::DuplicateAttempt => formatter.write_str("attempt ID re-registered with new spec"),
            Self::SnapshotConflict => formatter.write_str("snapshot ID rebound to new bytes"),
            Self::InvalidSnapshotReceipt { reason } => {
                write!(formatter, "invalid snapshot receipt: {reason}")
            }
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key reused for new bytes")
            }
            Self::InvalidArtifactPublicationPlan { reason } => {
                write!(formatter, "invalid artifact publication plan: {reason}")
            }
            Self::ArtifactPublicationConflict { reason } => {
                write!(formatter, "artifact publication receipt conflict: {reason}")
            }
            Self::InvalidSemanticPublicationPlan { reason } => {
                write!(formatter, "invalid semantic publication plan: {reason}")
            }
            Self::SemanticPublicationConflict { reason } => {
                write!(formatter, "semantic publication receipt conflict: {reason}")
            }
            Self::GroupPublicationInFlight => {
                formatter.write_str("group membership is frozen by in-flight publication")
            }
            Self::InvalidGeneration => formatter.write_str("stale generation for durable object"),
            Self::InvalidAttemptState { state } => {
                write!(formatter, "attempt state {state:?} rejects the transition")
            }
            Self::TaskCancelled => formatter.write_str("cancelled task admits no new attempts"),
            Self::NotPermitHolder => formatter.write_str("caller mismatches the holder binding"),
            Self::PermitNotIssued => formatter.write_str("permit is not issued"),
            Self::StaleTaskHead => {
                formatter.write_str("current TaskHead differs from the permit expected head")
            }
            Self::FenceRegression => formatter.write_str("retry-fence epoch must never regress"),
            Self::EpochExhausted => formatter.write_str("monotonic epoch space exhausted"),
            Self::InvalidEffectSet { reason } => {
                write!(formatter, "invalid planned effect set: {reason}")
            }
            Self::EffectSlotNotFound => {
                formatter.write_str("effect slot was not declared for this permit")
            }
            Self::EffectPermitNotFound => {
                formatter.write_str("effect permit does not exist under task")
            }
            Self::PermitEpochMismatch => {
                formatter.write_str("permit epoch mismatches the outstanding permit")
            }
            Self::CancellationCommitted { cancel_epoch } => write!(
                formatter,
                "cancellation committed at cancel_epoch {cancel_epoch} fences this permit"
            ),
            Self::InvalidEffectSlotState { state } => {
                write!(
                    formatter,
                    "effect slot state {state:?} rejects the transition"
                )
            }
            Self::DispatchTokenMismatch => {
                formatter.write_str("dispatch token does not match the issued one-shot token")
            }
            Self::DispatchTokenConsumed => {
                formatter.write_str("dispatch token was already consumed")
            }
            Self::OutstandingEffectSlots { count } => {
                write!(
                    formatter,
                    "{count} declared effect slots still block permit closure"
                )
            }
            Self::ConditionNotBound => {
                formatter.write_str("slot has no pre-bound required condition digest")
            }
            Self::Quarantined => {
                formatter.write_str("active permit is a quarantine tombstone; reconcile first")
            }
            Self::AdoptionScopeViolation => {
                formatter.write_str("permit adoption scope is RECONCILE_CLOSE_OR_QUARANTINE_ONLY")
            }
            Self::EffectAlreadyClosed => {
                formatter.write_str("logical effect is already EFFECT_CLOSED in effect history")
            }
            Self::RequiredEffectUnsatisfied { effect_seq, reason } => {
                write!(
                    formatter,
                    "required effect slot {effect_seq} is unsatisfied: {reason}"
                )
            }
            Self::InvalidReconcileState { reason } => {
                write!(formatter, "invalid reconcile state: {reason}")
            }
            Self::HistoryConflict => {
                formatter.write_str("presented bytes conflict with lifecycle record")
            }
            Self::PermitHasEffects { count } => write!(
                formatter,
                "{count} slots closed with an effect; pre-effect closure is forbidden"
            ),
            Self::GroupNotFound => formatter.write_str("group does not exist under the task"),
            Self::DuplicateGroup => formatter.write_str("group ID re-registered with new spec"),
            Self::GroupCycle => formatter.write_str("parent binding would create a group cycle"),
            Self::GroupDepthExceeded => {
                formatter.write_str("child exceeds an ancestor group's max_depth")
            }
            Self::GroupFanoutExceeded => {
                formatter.write_str("group max_children bound is exhausted")
            }
            Self::GroupSealed => formatter.write_str("sealed group admits no new child"),
            Self::GroupNotOpen { state } => {
                write!(
                    formatter,
                    "group state {state:?} rejects the membership mutation"
                )
            }
            Self::InvalidGroupState { state } => {
                write!(formatter, "group state {state:?} rejects the transition")
            }
            Self::MembershipConflict => {
                formatter.write_str("membership root/policy/member identity drift")
            }
            Self::StaleMembershipGeneration { expected, current } => write!(
                formatter,
                "stale membership generation: expected {expected}, current {current}"
            ),
            Self::GroupMemberNotFound => formatter.write_str("member is not present in the group"),
            Self::UnsupportedGroupMode => {
                formatter.write_str("reserved group mode/member type is not producible")
            }
            Self::GroupQuarantinedChild => {
                formatter.write_str("quarantined member cannot be removed from the group")
            }
            Self::ParticipantRegistryNotFound => {
                formatter.write_str("task participant registry does not exist")
            }
            Self::ParticipantRegistryFrozen { state } => {
                write!(formatter, "participant registry state {state:?} is frozen")
            }
            Self::ParticipantRegistryCasMismatch => {
                formatter.write_str("participant registry compare-and-swap failed")
            }
            Self::ParticipantRegistryBindingMissing => {
                formatter.write_str("participant registry binding is missing")
            }
            Self::ParticipantRegistryBindingMismatch => {
                formatter.write_str("participant registry binding mismatch")
            }
            Self::ParticipantEndpointConflict => {
                formatter.write_str("participant endpoint identity or receipt conflicts")
            }
            Self::ParticipantRegistryFull => {
                formatter.write_str("task participant registry is full")
            }
            Self::TaskWriteSetConflict { reason } => {
                write!(formatter, "TaskWriteSet conflict: {reason}")
            }
            Self::TaskWriteSetReadConflict => {
                formatter.write_str("TaskWriteSet Artifact read set differs from authority")
            }
            Self::TaskWriteSetSemanticReadConflict => {
                formatter.write_str("TaskWriteSet Semantic read set differs from authority")
            }
            Self::TaskWriteSetResourceReservationConflict => {
                formatter.write_str("TaskWriteSet Resource Reservation differs from authority")
            }
            Self::TaskWriteSetNotFound => {
                formatter.write_str("verified TaskWriteSet does not exist")
            }
            Self::ArtifactParticipantAuthority(error) => {
                write!(
                    formatter,
                    "Artifact participant proof verification failed: {error}"
                )
            }
            Self::SemanticParticipantAuthority(error) => {
                write!(
                    formatter,
                    "Semantic participant proof verification failed: {error}"
                )
            }
            Self::ResourceParticipantAuthority(error) => {
                write!(
                    formatter,
                    "Resource participant proof verification failed: {error}"
                )
            }
            Self::ProcessParticipantAuthority(error) => {
                write!(
                    formatter,
                    "Process participant proof verification failed: {error}"
                )
            }
            Self::OperationParticipantAuthority(error) => {
                write!(
                    formatter,
                    "Operation participant proof verification failed: {error}"
                )
            }
            Self::ParticipantEndpointGenerationMismatch { expected, current } => write!(
                formatter,
                "participant endpoint generation mismatch: expected {expected}, current {current}"
            ),
            Self::InvalidGroupSpec { reason } => {
                write!(formatter, "invalid group spec: {reason}")
            }
        }
    }
}

impl Error for TaskStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::ArtifactParticipantAuthority(error) => Some(error),
            Self::SemanticParticipantAuthority(error) => Some(error),
            Self::ResourceParticipantAuthority(error) => Some(error),
            Self::ProcessParticipantAuthority(error) => Some(error),
            Self::OperationParticipantAuthority(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for TaskStoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
