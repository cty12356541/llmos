//! Durable `TaskGroup` organization layer (B-TASK-004 slice).
//!
//! This module implements the single-authority `TaskGroup` subset of
//! `[TASK-GROUP-001]` / `[TASK-GROUP-002]`: acyclic parent/child trees
//! with `max_depth`/`max_children` enforcement and an immutable
//! `group_policy_digest` binding, content-addressed membership with a
//! monotonic generation CAS (`membership_root = H(domain || canonical
//! sorted active member set)`), immutable per-member Admission/Removal
//! receipts (member type/id/generation/`ControlDomain` placeholder),
//! OPEN-only child admission (SEALED rejects new members), optional
//! attempt registration bound bit-for-bit to the group's membership
//! generation/root and policy digest (drift fails closed), schema-v5
//! WriteSet/CommitPermit/TaskCommitReceipt membership binding with
//! pre-dispatch/finalize drift fences, structural tree cancellation
//! (`[TASK-CANCEL-001]` / `[TASK-CANCEL-002]`), and a
//! derived aggregate state recomputed from child terminal states
//! (`[TASK-STATE-002]` subset) with the quarantine cap of
//! `[TASK-GROUP-002]`'s final clause.
//!
//! Explicitly out of scope (reserved, non-producible): `QUORUM`/`REDUCE`
//! completion semantics and reducer digests, `BEST_EFFORT` failure mode,
//! `AGENT_INSTANCE` members, `DETACH` execution (`[TASK-DETACH-001]`),
//! ResourceGroup/ResourceAccount enforcement (placeholder binding
//! fields only), Namespace delegation, TaskPlan/TaskNode
//! materialization, cross-authority federation, and signatures.

use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ReceiptId, ResourceGroupId, TaskAttemptId,
    TaskId,
};
use rusqlite::{Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::model::derive_closure_receipt_id;
use crate::store::{
    self, SqlRead, blob16, blob32, encode_u64, generation_from_blob, optional_blob16, u64_from_blob,
};
use crate::{
    AttemptRecord, AttemptRegistrationDecision, AttemptSpec, AttemptState, ClosedAttempt,
    TaskState, TaskStoreError,
};

macro_rules! local_id {
    ($name:ident, $doc:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        #[doc = $doc]
        pub struct $name([u8; 16]);

        impl $name {
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn into_bytes(self) -> [u8; 16] {
                self.0
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }
    };
}

local_id!(
    TaskGroupId,
    "Authority-scoped identity of one `TaskGroup` (crate-local; `nlos-types` is owned by another lane)."
);

/// Membership position bound into a `TaskWriteSet`/`CommitPermit` and
/// copied verbatim into its terminal task receipt (`[TASK-GROUP-002]`).
///
/// Ungrouped attempts carry `None`. For grouped attempts the authority
/// snapshots the group's current generation/root/policy when the permit is
/// issued and refuses terminalization if that position drifts while the
/// permit is active.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskGroupCommitBinding {
    pub group_id: TaskGroupId,
    pub membership_generation: u64,
    pub membership_root: [u8; 32],
    pub group_policy_digest: [u8; 32],
}

/// `TaskGroup` lifecycle (§25.1 state machine, producible subset).
///
/// Producible here: `Open` (registration collapses `CREATED → OPEN`),
/// `Sealed`, `Completed`, `Failed`, `Partial`, `Cancelled`. All other
/// variants are reserved and cannot be produced in this slice;
/// intermediate `CancelRequested`/`Cancelling` collapse into the single
/// cancel transaction, and `EffectUnknown` administrative termination is
/// represented as `Partial` (see evidence doc §3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupState {
    Open,
    Sealed,
    /// Reserved; collapses into the cancel transaction here.
    CancelRequested,
    /// Reserved; collapses into the cancel transaction here.
    Cancelling,
    /// Reserved for the quiescence-drain slice.
    Quiescing,
    Completed,
    Failed,
    Partial,
    Cancelled,
    /// Reserved for the host-uncertainty slice.
    Uncertain,
    /// Reserved for the recovery slice.
    Recovering,
    /// Reserved for the integrity-failure slice.
    Quarantined,
    /// Reserved; administrative `EFFECT_UNKNOWN` termination is recorded
    /// as `Partial` in this slice.
    EffectUnknown,
}

impl GroupState {
    /// Whether the group admits new children (`[TASK-GROUP-002]`).
    #[must_use]
    pub const fn admits_children(self) -> bool {
        matches!(self, Self::Open)
    }

    /// Whether the group has reached a state this slice never leaves.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Partial | Self::Cancelled
        )
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Open => 0,
            Self::Sealed => 1,
            Self::CancelRequested => 2,
            Self::Cancelling => 3,
            Self::Quiescing => 4,
            Self::Completed => 5,
            Self::Failed => 6,
            Self::Partial => 7,
            Self::Cancelled => 8,
            Self::Uncertain => 9,
            Self::Recovering => 10,
            Self::Quarantined => 11,
            Self::EffectUnknown => 12,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Open),
            1 => Ok(Self::Sealed),
            2 => Ok(Self::CancelRequested),
            3 => Ok(Self::Cancelling),
            4 => Ok(Self::Quiescing),
            5 => Ok(Self::Completed),
            6 => Ok(Self::Failed),
            7 => Ok(Self::Partial),
            8 => Ok(Self::Cancelled),
            9 => Ok(Self::Uncertain),
            10 => Ok(Self::Recovering),
            11 => Ok(Self::Quarantined),
            12 => Ok(Self::EffectUnknown),
            _ => Err(TaskStoreError::CorruptRecord("unknown group state")),
        }
    }
}

/// How a group's children combine into its terminal state.
///
/// `Quorum` and `Reduce` are reserved (`[TASK-GROUP-003]` is out of
/// scope): registration with either fails closed with
/// [`TaskStoreError::UnsupportedGroupMode`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompletionMode {
    /// Terminal-success required from every active child.
    All,
    /// One terminal-success child completes the group.
    Any,
    /// Reserved; not producible in this slice.
    Quorum,
    /// Reserved; not producible in this slice.
    Reduce,
}

impl CompletionMode {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::All => 0,
            Self::Any => 1,
            Self::Quorum => 2,
            Self::Reduce => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::All),
            1 => Ok(Self::Any),
            2 => Ok(Self::Quorum),
            3 => Ok(Self::Reduce),
            _ => Err(TaskStoreError::CorruptRecord("unknown completion mode")),
        }
    }
}

/// Pre-committed child-failure policy (`[TASK-GROUP-001]`).
///
/// Placeholder semantics (see evidence doc §3): `FailFast` derives
/// `Failed` as soon as any child fails and propagates cancellation to
/// the remaining non-terminal descendants in the same transaction;
/// `CollectAll` waits for every child to reach a terminal state before
/// deriving `Failed`; `Isolate` bulkheads child failure — the group
/// derives `Partial` when at least one child succeeded and `Failed`
/// when none did. `BestEffort` is reserved and refused at registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureMode {
    FailFast,
    CollectAll,
    Isolate,
    /// Reserved; not producible in this slice.
    BestEffort,
}

impl FailureMode {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::FailFast => 0,
            Self::CollectAll => 1,
            Self::Isolate => 2,
            Self::BestEffort => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::FailFast),
            1 => Ok(Self::CollectAll),
            2 => Ok(Self::Isolate),
            3 => Ok(Self::BestEffort),
            _ => Err(TaskStoreError::CorruptRecord("unknown failure mode")),
        }
    }
}

/// Kind of a `TaskGroup` member (`TaskGroupRecord.members[].member_type`).
///
/// `AgentInstance` is reserved (B-PROCESS prerequisite): admitting one
/// fails closed with [`TaskStoreError::UnsupportedGroupMode`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupMemberType {
    ChildGroup,
    TaskAttempt,
    /// Reserved; not producible in this slice.
    AgentInstance,
}

impl GroupMemberType {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::ChildGroup => 0,
            Self::TaskAttempt => 1,
            Self::AgentInstance => 2,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::ChildGroup),
            1 => Ok(Self::TaskAttempt),
            2 => Ok(Self::AgentInstance),
            _ => Err(TaskStoreError::CorruptRecord("unknown group member type")),
        }
    }
}

/// Whether a member row is part of the current membership set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MembershipState {
    Active,
    Removed,
}

impl MembershipState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Active => 0,
            Self::Removed => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Active),
            1 => Ok(Self::Removed),
            _ => Err(TaskStoreError::CorruptRecord("unknown membership state")),
        }
    }
}

/// Kind of a durable membership receipt (`[TASK-GROUP-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupReceiptKind {
    Admission,
    Removal,
}

impl GroupReceiptKind {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Admission => 0,
            Self::Removal => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Admission),
            1 => Ok(Self::Removal),
            _ => Err(TaskStoreError::CorruptRecord("unknown group receipt kind")),
        }
    }
}

/// Durable specification of a `TaskGroup` registration.
///
/// Registration is idempotent on `group_id`: repeating the exact same
/// specification returns `Existing`; reusing the ID with different bytes
/// is rejected fail-closed. A child group (`parent_group_id = Some`) is
/// born and admitted into the parent's membership in ONE transaction
/// (`[TASK-GROUP-002]` `BirthDecision` subset).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupSpec {
    pub group_id: TaskGroupId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    /// `None` registers the task's root group; exactly one root per task
    /// is representable (enforced by a partial unique index).
    pub parent_group_id: Option<TaskGroupId>,
    /// Caller-supplied immutable policy binding placeholder.
    pub group_policy_digest: [u8; 32],
    pub completion_mode: CompletionMode,
    pub failure_mode: FailureMode,
    /// Maximum number of simultaneously ACTIVE members; fail-closed.
    pub max_children: u64,
    /// Maximum depth of any descendant group relative to this group
    /// (this group is depth 0 of its own subtree); fail-closed.
    pub max_depth: u64,
    /// Placeholder binding only; no `ResourceGroup` enforcement here.
    pub resource_group_id: Option<ResourceGroupId>,
    /// Placeholder binding only; no `ResourceAccount` enforcement here.
    pub resource_account_digest: Option<[u8; 32]>,
    pub cancellation_scope_id: CancellationScopeId,
    pub registered_at_ms: i64,
}

/// Durable view of one `TaskGroup` (`TaskGroupRecord` subset).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupRecord {
    pub group_id: TaskGroupId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub parent_group_id: Option<TaskGroupId>,
    /// Absolute depth from the tree root (root = 0).
    pub depth: u64,
    pub state: GroupState,
    pub membership_generation: u64,
    pub membership_root: [u8; 32],
    pub group_policy_digest: [u8; 32],
    pub completion_mode: CompletionMode,
    pub failure_mode: FailureMode,
    pub max_children: u64,
    pub max_depth: u64,
    pub cancel_epoch: u64,
    /// Monotonic transition counter of the derived/persisted state.
    pub state_seq: u64,
    pub resource_group_id: Option<ResourceGroupId>,
    pub resource_account_digest: Option<[u8; 32]>,
    pub cancellation_scope_id: CancellationScopeId,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one membership row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupMemberRecord {
    pub group_id: TaskGroupId,
    pub member_type: GroupMemberType,
    /// Raw member identity bytes: a [`TaskGroupId`] for
    /// [`GroupMemberType::ChildGroup`], a [`TaskAttemptId`] for
    /// [`GroupMemberType::TaskAttempt`].
    pub member_id: [u8; 16],
    pub member_generation: Generation,
    /// `ControlDomain` placeholder (`[TASK-GROUP-002]`); no
    /// `ControlDomain` authority exists in this slice.
    pub control_domain_id: Option<[u8; 16]>,
    /// `DETACH` placeholder (`[TASK-DETACH-001]`): reserved, admission
    /// with `detached = true` fails closed in this slice.
    pub detached: bool,
    pub membership_state: MembershipState,
    /// The membership generation at which this member was admitted.
    pub membership_generation: u64,
    pub admission_receipt_id: ReceiptId,
    pub removal_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable Admission/Removal receipt (`[TASK-GROUP-002]`, immutable).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GroupAdmissionReceiptRecord {
    pub receipt_id: ReceiptId,
    pub group_id: TaskGroupId,
    pub kind: GroupReceiptKind,
    pub member_type: GroupMemberType,
    pub member_id: [u8; 16],
    pub member_generation: Generation,
    pub control_domain_id: Option<[u8; 16]>,
    pub membership_generation_after: u64,
    pub membership_root_after: [u8; 32],
    pub created_at_ms: i64,
}

/// The group binding an attempt registered via
/// [`SqliteTaskAuthority::register_attempt_in_group`](crate::SqliteTaskAuthority::register_attempt_in_group)
/// is checked against. All three fields are compared bit-for-bit with
/// the group's current durable values; any drift fails closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupBinding {
    pub group_id: TaskGroupId,
    /// Expected CURRENT membership generation (pre-admission drift
    /// fence); the persisted binding records the post-admission
    /// generation (see evidence doc §3).
    pub expected_membership_generation: u64,
    pub expected_membership_root: [u8; 32],
    pub expected_group_policy_digest: [u8; 32],
}

/// Durable membership binding recorded on a group-bound attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptGroupBindingRecord {
    pub attempt_id: TaskAttemptId,
    pub task_id: TaskId,
    pub group_id: TaskGroupId,
    /// Post-admission membership generation this attempt belongs to.
    pub membership_generation: u64,
    pub membership_root: [u8; 32],
    pub group_policy_digest: [u8; 32],
    pub created_at_ms: i64,
}

/// Decision of an idempotent `TaskGroup` registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupRegistrationDecision {
    Created(TaskGroupId),
    Existing(TaskGroupId),
}

impl GroupRegistrationDecision {
    #[must_use]
    pub const fn group_id(self) -> TaskGroupId {
        match self {
            Self::Created(group_id) | Self::Existing(group_id) => group_id,
        }
    }
}

/// Result of a successful `register_attempt_in_group`: the attempt
/// registration decision plus the membership position it was admitted at.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptGroupRegistration {
    pub decision: AttemptRegistrationDecision,
    pub binding: AttemptGroupBindingRecord,
    pub admission_receipt_id: ReceiptId,
}

/// Identity of a member for removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupMemberRef {
    pub member_type: GroupMemberType,
    pub member_id: [u8; 16],
    pub member_generation: Generation,
}

/// A membership removal request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RemoveMemberRequest {
    pub group_id: TaskGroupId,
    pub member: GroupMemberRef,
    pub removed_at_ms: i64,
}

/// Decision of a membership removal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemovalDecision {
    Removed(Box<GroupAdmissionReceiptRecord>),
    /// The member was already removed; the original removal receipt.
    Replayed(Box<GroupAdmissionReceiptRecord>),
}

/// A `TaskGroup` cancellation request (`[TASK-CANCEL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GroupCancelRequest {
    pub group_id: TaskGroupId,
    pub idempotency_key: IdempotencyKey,
    pub requested_at_ms: i64,
}

/// Linearized decision of a `TaskGroup` cancellation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GroupCancelDecision {
    /// `cancel_epoch` advanced exactly once and every non-terminal
    /// descendant transitioned in the same transaction.
    Applied {
        cancel_epoch: u64,
        closed_attempts: Vec<ClosedAttempt>,
        cancelled_groups: Vec<TaskGroupId>,
    },
    /// Exact replay of the original cancellation key; nothing re-applied.
    Replayed { cancel_epoch: u64 },
    /// A different key arrived after cancellation was already committed.
    AlreadyCancelled { cancel_epoch: u64 },
}

/// Computes the domain-separated placeholder digest of an empty group
/// membership set: `SHA-256("llmos/task-group-membership/v1")`.
#[must_use]
pub fn empty_group_membership_root() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-group-membership/v1");
    hasher.finalize().into()
}

/// Content-addressed membership root: domain-separated SHA-256 over the
/// canonical sorted ACTIVE member set
/// (`[TASK-GROUP-002]`).
///
/// Members are sorted by `(member_type code, member_id bytes)`; each
/// entry contributes `type(1) || member_id(16) || generation(8) ||
/// control_domain flag+bytes(1|17) || detached(1)`. The fixed-width
/// placeholder encoding stands in for deterministic CBOR, consistent
/// with the rest of this crate.
#[must_use]
pub fn membership_root_of(members: &[GroupMemberRecord]) -> [u8; 32] {
    let mut sorted: Vec<&GroupMemberRecord> = members
        .iter()
        .filter(|member| member.membership_state == MembershipState::Active)
        .collect();
    sorted.sort_by_key(|member| (member.member_type.code(), member.member_id));
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-group-membership/v1");
    for member in sorted {
        hasher.update(member.member_type.code().to_be_bytes());
        hasher.update(member.member_id);
        hasher.update(member.member_generation.get().to_be_bytes());
        match member.control_domain_id {
            Some(control_domain) => {
                hasher.update([1u8]);
                hasher.update(control_domain);
            }
            None => hasher.update([0u8]),
        }
        hasher.update([u8::from(member.detached)]);
    }
    hasher.finalize().into()
}

fn sha256_prefix16(domain: &str, parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(part);
    }
    let full: [u8; 32] = hasher.finalize().into();
    let mut prefix = [0u8; 16];
    prefix.copy_from_slice(&full[..16]);
    prefix
}

/// Deterministic admission receipt identity bound to the group, the
/// membership generation it produces, and the admitted member.
pub(crate) fn derive_group_admission_receipt_id(
    group_id: TaskGroupId,
    generation_after: u64,
    member_type: GroupMemberType,
    member_id: &[u8; 16],
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-group-admission/v1",
        &[
            group_id.as_bytes(),
            &generation_after.to_be_bytes(),
            &member_type.code().to_be_bytes(),
            member_id,
        ],
    ))
}

/// Deterministic removal receipt identity bound to the group, the
/// membership generation it produces, and the removed member.
pub(crate) fn derive_group_removal_receipt_id(
    group_id: TaskGroupId,
    generation_after: u64,
    member_type: GroupMemberType,
    member_id: &[u8; 16],
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-group-removal/v1",
        &[
            group_id.as_bytes(),
            &generation_after.to_be_bytes(),
            &member_type.code().to_be_bytes(),
            member_id,
        ],
    ))
}

pub(crate) const SCHEMA_V4_SQL: &str =
    "CREATE TABLE task_groups (
            group_id BLOB PRIMARY KEY NOT NULL CHECK(length(group_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            parent_group_id BLOB CHECK(parent_group_id IS NULL OR length(parent_group_id) = 16),
            depth INTEGER NOT NULL,
            group_state INTEGER NOT NULL,
            membership_generation BLOB NOT NULL CHECK(length(membership_generation) = 8),
            membership_root BLOB NOT NULL CHECK(length(membership_root) = 32),
            group_policy_digest BLOB NOT NULL CHECK(length(group_policy_digest) = 32),
            completion_mode INTEGER NOT NULL,
            failure_mode INTEGER NOT NULL,
            max_children INTEGER NOT NULL,
            max_depth INTEGER NOT NULL,
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            state_seq INTEGER NOT NULL,
            resource_group_id BLOB CHECK(resource_group_id IS NULL OR length(resource_group_id) = 16),
            resource_account_digest BLOB CHECK(resource_account_digest IS NULL OR length(resource_account_digest) = 32),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id),
            FOREIGN KEY(parent_group_id) REFERENCES task_groups(group_id)
        ) STRICT;

        -- One root group per task (`Task ─owns→ root TaskGroup`).
        CREATE UNIQUE INDEX task_groups_single_root
            ON task_groups(task_id) WHERE parent_group_id IS NULL;

        CREATE INDEX task_groups_by_parent
            ON task_groups(parent_group_id);

        CREATE TABLE task_group_members (
            group_id BLOB NOT NULL CHECK(length(group_id) = 16),
            member_type INTEGER NOT NULL,
            member_id BLOB NOT NULL CHECK(length(member_id) = 16),
            member_generation BLOB NOT NULL CHECK(length(member_generation) = 8),
            control_domain_id BLOB CHECK(control_domain_id IS NULL OR length(control_domain_id) = 16),
            detached INTEGER NOT NULL,
            membership_state INTEGER NOT NULL,
            membership_generation BLOB NOT NULL CHECK(length(membership_generation) = 8),
            admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
            removal_receipt_id BLOB CHECK(removal_receipt_id IS NULL OR length(removal_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(group_id, member_type, member_id),
            FOREIGN KEY(group_id) REFERENCES task_groups(group_id)
        ) STRICT;

        CREATE TABLE task_group_admission_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            group_id BLOB NOT NULL CHECK(length(group_id) = 16),
            receipt_kind INTEGER NOT NULL,
            member_type INTEGER NOT NULL,
            member_id BLOB NOT NULL CHECK(length(member_id) = 16),
            member_generation BLOB NOT NULL CHECK(length(member_generation) = 8),
            control_domain_id BLOB CHECK(control_domain_id IS NULL OR length(control_domain_id) = 16),
            membership_generation_after BLOB NOT NULL CHECK(length(membership_generation_after) = 8),
            membership_root_after BLOB NOT NULL CHECK(length(membership_root_after) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(group_id) REFERENCES task_groups(group_id)
        ) STRICT;

        CREATE INDEX task_group_admission_receipts_by_group
            ON task_group_admission_receipts(group_id, receipt_kind);

        CREATE TRIGGER task_group_admission_receipt_is_immutable
        BEFORE UPDATE ON task_group_admission_receipts
        BEGIN
            SELECT RAISE(ABORT, 'group admission receipt is immutable');
        END;

        CREATE TABLE task_group_cancels (
            group_id BLOB PRIMARY KEY NOT NULL CHECK(length(group_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(group_id) REFERENCES task_groups(group_id)
        ) STRICT;

        CREATE TABLE task_attempt_group_bindings (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            group_id BLOB NOT NULL CHECK(length(group_id) = 16),
            membership_generation BLOB NOT NULL CHECK(length(membership_generation) = 8),
            membership_root BLOB NOT NULL CHECK(length(membership_root) = 32),
            group_policy_digest BLOB NOT NULL CHECK(length(group_policy_digest) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(attempt_id) REFERENCES task_attempts(attempt_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id),
            FOREIGN KEY(group_id) REFERENCES task_groups(group_id)
        ) STRICT;

        CREATE TRIGGER task_attempt_group_binding_is_immutable
        BEFORE UPDATE ON task_attempt_group_bindings
        BEGIN
            SELECT RAISE(ABORT, 'attempt group binding is immutable');
        END;

        PRAGMA user_version = 4;";

pub(crate) struct StoredGroup {
    pub(crate) record: GroupRecord,
    revision: i64,
}

struct StoredGroupCancel {
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
}

fn insert_group(transaction: &Transaction<'_>, record: &GroupRecord) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_groups (
            group_id, task_id, task_generation, parent_group_id, depth,
            group_state, membership_generation, membership_root,
            group_policy_digest, completion_mode, failure_mode,
            max_children, max_depth, cancel_epoch, state_seq,
            resource_group_id, resource_account_digest, cancellation_scope_id,
            revision, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                   ?15, ?16, ?17, ?18, 0, ?19, ?20)",
        params![
            record.group_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            record
                .parent_group_id
                .map(TaskGroupId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            i64::try_from(record.depth).map_err(|_| TaskStoreError::InvalidGroupSpec {
                reason: "depth overflows i64"
            })?,
            record.state.code(),
            encode_u64(record.membership_generation).as_slice(),
            record.membership_root.as_slice(),
            record.group_policy_digest.as_slice(),
            record.completion_mode.code(),
            record.failure_mode.code(),
            i64::try_from(record.max_children).map_err(|_| TaskStoreError::InvalidGroupSpec {
                reason: "max_children overflows i64"
            })?,
            i64::try_from(record.max_depth).map_err(|_| TaskStoreError::InvalidGroupSpec {
                reason: "max_depth overflows i64"
            })?,
            encode_u64(record.cancel_epoch).as_slice(),
            i64::try_from(record.state_seq).map_err(|_| TaskStoreError::InvalidGroupSpec {
                reason: "state_seq overflows i64"
            })?,
            record
                .resource_group_id
                .map(ResourceGroupId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            record
                .resource_account_digest
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            record.cancellation_scope_id.as_bytes().as_slice(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

fn update_group(
    transaction: &Transaction<'_>,
    group: &StoredGroup,
    now_ms: i64,
    mutate: impl FnOnce(&mut GroupRecord),
) -> Result<(), TaskStoreError> {
    let mut record = group.record.clone();
    mutate(&mut record);
    let changed = transaction.execute(
        "UPDATE task_groups SET
            group_state = ?1, membership_generation = ?2, membership_root = ?3,
            cancel_epoch = ?4, state_seq = ?5, updated_at_ms = ?6,
            revision = revision + 1
         WHERE group_id = ?7 AND revision = ?8",
        params![
            record.state.code(),
            encode_u64(record.membership_generation).as_slice(),
            record.membership_root.as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            i64::try_from(record.state_seq)
                .map_err(|_| { TaskStoreError::CorruptRecord("group state_seq overflows i64") })?,
            now_ms,
            record.group_id.as_bytes().as_slice(),
            group.revision,
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "group revision compare-and-swap failed",
        ));
    }
    Ok(())
}

const GROUP_COLUMNS: &str = "group_id, task_id, task_generation, parent_group_id, depth,
     group_state, membership_generation, membership_root, group_policy_digest,
     completion_mode, failure_mode, max_children, max_depth, cancel_epoch,
     state_seq, resource_group_id, resource_account_digest, cancellation_scope_id,
     revision, created_at_ms, updated_at_ms";

fn load_group_optional(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<Option<StoredGroup>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {GROUP_COLUMNS} FROM task_groups WHERE group_id = ?1"
    ))?;
    let mut rows = statement.query([group_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_group_row).transpose()
}

fn load_group(source: &impl SqlRead, group_id: TaskGroupId) -> Result<StoredGroup, TaskStoreError> {
    load_group_optional(source, group_id)?.ok_or(TaskStoreError::GroupNotFound)
}

fn load_root_group(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<StoredGroup>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {GROUP_COLUMNS} FROM task_groups
         WHERE task_id = ?1 AND parent_group_id IS NULL"
    ))?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_group_row).transpose()
}

#[allow(clippy::too_many_lines)]
fn decode_group_row(row: &rusqlite::Row<'_>) -> Result<StoredGroup, TaskStoreError> {
    let depth: i64 = row.get(4)?;
    let max_children: i64 = row.get(11)?;
    let max_depth: i64 = row.get(12)?;
    let state_seq: i64 = row.get(14)?;
    let revision: i64 = row.get(18)?;
    if depth < 0 || max_children < 0 || max_depth < 0 || state_seq < 0 || revision < 0 {
        return Err(TaskStoreError::CorruptRecord("negative group counter"));
    }
    Ok(StoredGroup {
        record: GroupRecord {
            group_id: TaskGroupId::from_bytes(blob16(row, 0)?),
            task_id: TaskId::from_bytes(blob16(row, 1)?),
            task_generation: generation_from_blob(row, 2)?,
            parent_group_id: optional_blob16(row, 3)?.map(TaskGroupId::from_bytes),
            depth: u64::try_from(depth)
                .map_err(|_| TaskStoreError::CorruptRecord("group depth overflows"))?,
            state: GroupState::from_code(row.get(5)?)?,
            membership_generation: u64_from_blob(row, 6)?,
            membership_root: blob32(row, 7)?,
            group_policy_digest: blob32(row, 8)?,
            completion_mode: CompletionMode::from_code(row.get(9)?)?,
            failure_mode: FailureMode::from_code(row.get(10)?)?,
            max_children: u64::try_from(max_children)
                .map_err(|_| TaskStoreError::CorruptRecord("max_children overflows"))?,
            max_depth: u64::try_from(max_depth)
                .map_err(|_| TaskStoreError::CorruptRecord("max_depth overflows"))?,
            cancel_epoch: u64_from_blob(row, 13)?,
            state_seq: u64::try_from(state_seq)
                .map_err(|_| TaskStoreError::CorruptRecord("state_seq overflows"))?,
            resource_group_id: optional_blob16(row, 15)?.map(ResourceGroupId::from_bytes),
            resource_account_digest: match row.get::<_, Option<Vec<u8>>>(16)? {
                Some(bytes) => Some(bytes.try_into().map_err(|_| {
                    TaskStoreError::CorruptRecord("expected 32-byte account digest")
                })?),
                None => None,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(blob16(row, 17)?),
            created_at_ms: row.get(19)?,
            updated_at_ms: row.get(20)?,
        },
        revision,
    })
}

fn insert_member(
    transaction: &Transaction<'_>,
    record: &GroupMemberRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_group_members (
            group_id, member_type, member_id, member_generation, control_domain_id,
            detached, membership_state, membership_generation, admission_receipt_id,
            removal_receipt_id, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11)",
        params![
            record.group_id.as_bytes().as_slice(),
            record.member_type.code(),
            record.member_id.as_slice(),
            encode_u64(record.member_generation.get()).as_slice(),
            record.control_domain_id.as_ref().map(<[u8; 16]>::as_slice),
            i64::from(record.detached),
            record.membership_state.code(),
            encode_u64(record.membership_generation).as_slice(),
            record.admission_receipt_id.as_bytes().as_slice(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

fn set_member_removed(
    transaction: &Transaction<'_>,
    member: &GroupMemberRecord,
    removal_receipt_id: ReceiptId,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_group_members
         SET membership_state = ?1, removal_receipt_id = ?2, updated_at_ms = ?3
         WHERE group_id = ?4 AND member_type = ?5 AND member_id = ?6
           AND membership_state = ?7",
        params![
            MembershipState::Removed.code(),
            removal_receipt_id.as_bytes().as_slice(),
            now_ms,
            member.group_id.as_bytes().as_slice(),
            member.member_type.code(),
            member.member_id.as_slice(),
            MembershipState::Active.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "member removal compare-and-swap failed",
        ));
    }
    Ok(())
}

const MEMBER_COLUMNS: &str = "group_id, member_type, member_id, member_generation,
     control_domain_id, detached, membership_state, membership_generation,
     admission_receipt_id, removal_receipt_id, created_at_ms, updated_at_ms";

fn decode_member_row(row: &rusqlite::Row<'_>) -> Result<GroupMemberRecord, TaskStoreError> {
    let detached: i64 = row.get(5)?;
    Ok(GroupMemberRecord {
        group_id: TaskGroupId::from_bytes(blob16(row, 0)?),
        member_type: GroupMemberType::from_code(row.get(1)?)?,
        member_id: blob16(row, 2)?,
        member_generation: generation_from_blob(row, 3)?,
        control_domain_id: optional_blob16(row, 4)?,
        detached: detached != 0,
        membership_state: MembershipState::from_code(row.get(6)?)?,
        membership_generation: u64_from_blob(row, 7)?,
        admission_receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
        removal_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(10)?,
        updated_at_ms: row.get(11)?,
    })
}

fn load_member_optional(
    source: &impl SqlRead,
    group_id: TaskGroupId,
    member_type: GroupMemberType,
    member_id: &[u8; 16],
) -> Result<Option<GroupMemberRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {MEMBER_COLUMNS} FROM task_group_members
         WHERE group_id = ?1 AND member_type = ?2 AND member_id = ?3"
    ))?;
    let mut rows = statement.query(params![
        group_id.as_bytes().as_slice(),
        member_type.code(),
        member_id.as_slice(),
    ])?;
    rows.next()?.map(decode_member_row).transpose()
}

fn list_members(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<Vec<GroupMemberRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {MEMBER_COLUMNS} FROM task_group_members
         WHERE group_id = ?1 ORDER BY member_type, member_id"
    ))?;
    let mut rows = statement.query([group_id.as_bytes().as_slice()])?;
    let mut members = Vec::new();
    while let Some(row) = rows.next()? {
        members.push(decode_member_row(row)?);
    }
    Ok(members)
}

fn count_active_members(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<u64, TaskStoreError> {
    let count: i64 = source
        .prepare_statement(
            "SELECT COUNT(*) FROM task_group_members
             WHERE group_id = ?1 AND membership_state = ?2",
        )?
        .query_row(
            params![
                group_id.as_bytes().as_slice(),
                MembershipState::Active.code()
            ],
            |row| row.get(0),
        )?;
    u64::try_from(count).map_err(|_| TaskStoreError::CorruptRecord("negative member count"))
}

fn insert_group_receipt(
    transaction: &Transaction<'_>,
    record: &GroupAdmissionReceiptRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_group_admission_receipts (
            receipt_id, group_id, receipt_kind, member_type, member_id,
            member_generation, control_domain_id, membership_generation_after,
            membership_root_after, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.group_id.as_bytes().as_slice(),
            record.kind.code(),
            record.member_type.code(),
            record.member_id.as_slice(),
            encode_u64(record.member_generation.get()).as_slice(),
            record.control_domain_id.as_ref().map(<[u8; 16]>::as_slice),
            encode_u64(record.membership_generation_after).as_slice(),
            record.membership_root_after.as_slice(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

const GROUP_RECEIPT_COLUMNS: &str = "receipt_id, group_id, receipt_kind, member_type, member_id,
     member_generation, control_domain_id, membership_generation_after,
     membership_root_after, created_at_ms";

fn decode_group_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<GroupAdmissionReceiptRecord, TaskStoreError> {
    Ok(GroupAdmissionReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        group_id: TaskGroupId::from_bytes(blob16(row, 1)?),
        kind: GroupReceiptKind::from_code(row.get(2)?)?,
        member_type: GroupMemberType::from_code(row.get(3)?)?,
        member_id: blob16(row, 4)?,
        member_generation: generation_from_blob(row, 5)?,
        control_domain_id: optional_blob16(row, 6)?,
        membership_generation_after: u64_from_blob(row, 7)?,
        membership_root_after: blob32(row, 8)?,
        created_at_ms: row.get(9)?,
    })
}

fn list_group_receipts(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<Vec<GroupAdmissionReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {GROUP_RECEIPT_COLUMNS} FROM task_group_admission_receipts
         WHERE group_id = ?1 ORDER BY membership_generation_after, receipt_kind"
    ))?;
    let mut rows = statement.query([group_id.as_bytes().as_slice()])?;
    let mut receipts = Vec::new();
    while let Some(row) = rows.next()? {
        receipts.push(decode_group_receipt_row(row)?);
    }
    Ok(receipts)
}

fn insert_group_cancel(
    transaction: &Transaction<'_>,
    group_id: TaskGroupId,
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_group_cancels (
            group_id, idempotency_key, cancel_epoch_after, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            group_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            encode_u64(cancel_epoch_after).as_slice(),
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_group_cancel(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<Option<StoredGroupCancel>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT idempotency_key, cancel_epoch_after FROM task_group_cancels
         WHERE group_id = ?1",
    )?;
    let mut rows = statement.query([group_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            Ok(StoredGroupCancel {
                idempotency_key: IdempotencyKey::from_bytes(blob16(row, 0)?),
                cancel_epoch_after: u64_from_blob(row, 1)?,
            })
        })
        .transpose()
}

fn insert_attempt_binding(
    transaction: &Transaction<'_>,
    record: &AttemptGroupBindingRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_attempt_group_bindings (
            attempt_id, task_id, group_id, membership_generation, membership_root,
            group_policy_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.attempt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.group_id.as_bytes().as_slice(),
            encode_u64(record.membership_generation).as_slice(),
            record.membership_root.as_slice(),
            record.group_policy_digest.as_slice(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn load_attempt_binding(
    source: &impl SqlRead,
    attempt_id: TaskAttemptId,
) -> Result<Option<AttemptGroupBindingRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT attempt_id, task_id, group_id, membership_generation, membership_root,
                group_policy_digest, created_at_ms
         FROM task_attempt_group_bindings WHERE attempt_id = ?1",
    )?;
    let mut rows = statement.query([attempt_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            Ok(AttemptGroupBindingRecord {
                attempt_id: TaskAttemptId::from_bytes(blob16(row, 0)?),
                task_id: TaskId::from_bytes(blob16(row, 1)?),
                group_id: TaskGroupId::from_bytes(blob16(row, 2)?),
                membership_generation: u64_from_blob(row, 3)?,
                membership_root: blob32(row, 4)?,
                group_policy_digest: blob32(row, 5)?,
                created_at_ms: row.get(6)?,
            })
        })
        .transpose()
}

/// Captures the current membership position for a grouped attempt at
/// `CommitPermit` issuance. The attempt must still be an active member;
/// removal cannot be laundered into an ungrouped commit.
pub(crate) fn current_commit_binding(
    source: &impl SqlRead,
    attempt_id: TaskAttemptId,
) -> Result<Option<TaskGroupCommitBinding>, TaskStoreError> {
    let Some(admission) = load_attempt_binding(source, attempt_id)? else {
        return Ok(None);
    };
    let group = load_group(source, admission.group_id)?;
    let member = load_member_optional(
        source,
        admission.group_id,
        GroupMemberType::TaskAttempt,
        attempt_id.as_bytes(),
    )?
    .ok_or(TaskStoreError::GroupMemberNotFound)?;
    if member.membership_state != MembershipState::Active {
        return Err(TaskStoreError::MembershipConflict);
    }
    Ok(Some(TaskGroupCommitBinding {
        group_id: group.record.group_id,
        membership_generation: group.record.membership_generation,
        membership_root: group.record.membership_root,
        group_policy_digest: group.record.group_policy_digest,
    }))
}

/// Revalidates the exact group position captured by a live permit before
/// writing any terminal task receipt. A membership or policy change while
/// the permit is active fails closed and leaves the permit/head untouched.
pub(crate) fn validate_commit_binding(
    source: &impl SqlRead,
    attempt_id: TaskAttemptId,
    expected: Option<TaskGroupCommitBinding>,
) -> Result<(), TaskStoreError> {
    if current_commit_binding(source, attempt_id)? == expected {
        Ok(())
    } else {
        Err(TaskStoreError::MembershipConflict)
    }
}

fn attempt_has_quarantined_permit(
    source: &impl SqlRead,
    attempt_id: &[u8; 16],
) -> Result<bool, TaskStoreError> {
    let count: i64 = source
        .prepare_statement(
            "SELECT COUNT(*) FROM commit_permits
             WHERE attempt_id = ?1 AND permit_state = ?2",
        )?
        .query_row(
            params![
                attempt_id.as_slice(),
                crate::PermitState::Quarantined.code()
            ],
            |row| row.get(0),
        )?;
    Ok(count > 0)
}

/// Whether any descendant of the group holds quarantine evidence:
/// a member attempt with a `Quarantined` permit tombstone
/// (`[TASK-EFFECT-003]`) or a child group with quarantine in its own
/// subtree. Derived from child states; the tree is acyclic by
/// construction so the walk terminates.
fn subtree_quarantined(
    source: &impl SqlRead,
    group_id: TaskGroupId,
) -> Result<bool, TaskStoreError> {
    for member in list_members(source, group_id)? {
        if member.membership_state != MembershipState::Active {
            continue;
        }
        match member.member_type {
            GroupMemberType::TaskAttempt => {
                if attempt_has_quarantined_permit(source, &member.member_id)? {
                    return Ok(true);
                }
            }
            GroupMemberType::ChildGroup => {
                if subtree_quarantined(source, TaskGroupId::from_bytes(member.member_id))? {
                    return Ok(true);
                }
            }
            GroupMemberType::AgentInstance => {}
        }
    }
    Ok(false)
}

/// One member's aggregate-relevant classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildClass {
    Success,
    Failure,
    Cancelled,
    Partial,
    NonTerminal,
}

fn classify_group(state: GroupState) -> ChildClass {
    match state {
        GroupState::Completed => ChildClass::Success,
        GroupState::Failed => ChildClass::Failure,
        GroupState::Cancelled => ChildClass::Cancelled,
        GroupState::Partial => ChildClass::Partial,
        _ => ChildClass::NonTerminal,
    }
}

fn classify_attempt(state: AttemptState) -> ChildClass {
    match state {
        AttemptState::Committed => ChildClass::Success,
        AttemptState::Failed => ChildClass::Failure,
        AttemptState::Cancelled | AttemptState::Conflicted | AttemptState::Superseded => {
            ChildClass::Cancelled
        }
        _ => ChildClass::NonTerminal,
    }
}

/// What the derived aggregate decided for one refresh.
struct AggregateDerivation {
    target: Option<GroupState>,
    /// `FAIL_FAST`: cancel the remaining non-terminal descendants in the
    /// same transaction before persisting `Failed`.
    propagate_cancel: bool,
}

fn derive_aggregate(
    completion_mode: CompletionMode,
    failure_mode: FailureMode,
    classes: &[ChildClass],
    quarantined: bool,
) -> AggregateDerivation {
    let any = |class: ChildClass| classes.contains(&class);
    let all_success = classes.iter().all(|class| *class == ChildClass::Success);
    let base = match completion_mode {
        CompletionMode::All => {
            if failure_mode == FailureMode::FailFast
                && any(ChildClass::Failure)
                && any(ChildClass::NonTerminal)
            {
                return AggregateDerivation {
                    target: Some(GroupState::Failed),
                    propagate_cancel: true,
                };
            }
            if any(ChildClass::NonTerminal) {
                None
            } else if all_success {
                Some(GroupState::Completed)
            } else if any(ChildClass::Failure) {
                match failure_mode {
                    FailureMode::FailFast | FailureMode::CollectAll => Some(GroupState::Failed),
                    FailureMode::Isolate => {
                        if any(ChildClass::Success) {
                            Some(GroupState::Partial)
                        } else {
                            Some(GroupState::Failed)
                        }
                    }
                    FailureMode::BestEffort => None,
                }
            } else {
                Some(GroupState::Partial)
            }
        }
        CompletionMode::Any => {
            if any(ChildClass::Success) {
                Some(GroupState::Completed)
            } else if any(ChildClass::NonTerminal) {
                None
            } else if any(ChildClass::Failure) {
                Some(GroupState::Failed)
            } else if any(ChildClass::Partial) {
                Some(GroupState::Partial)
            } else {
                Some(GroupState::Cancelled)
            }
        }
        CompletionMode::Quorum | CompletionMode::Reduce => None,
    };
    // `[TASK-GROUP-002]` final clause: quarantine anywhere in the subtree
    // caps the administrative terminal at PARTIAL, never COMPLETED.
    let target = match (base, quarantined) {
        (Some(GroupState::Completed), true) => Some(GroupState::Partial),
        (other, _) => other,
    };
    AggregateDerivation {
        target,
        propagate_cancel: false,
    }
}

/// Closes every non-terminal descendant of `group` per the B-TASK-001
/// cancel semantics (closure receipts, head unchanged), recursively.
/// Child groups advance their own `cancel_epoch` exactly once per
/// propagation and land `Cancelled` (or `Partial` when their subtree
/// holds quarantine evidence); open pre-permit member attempts close
/// with `CANCELLED_BEFORE_EFFECT` closure receipts. Permit-holding
/// attempts stay untouched (permit-first linearization,
/// `[TASK-CANCEL-003]`), terminal children are never re-touched, and
/// detached members (reserved) are skipped by design.
#[allow(clippy::too_many_arguments)]
fn propagate_cancel(
    transaction: &Transaction<'_>,
    task: &crate::TaskRecord,
    group: &StoredGroup,
    cancel_epoch: u64,
    now_ms: i64,
    closed_attempts: &mut Vec<ClosedAttempt>,
    cancelled_groups: &mut Vec<TaskGroupId>,
) -> Result<(), TaskStoreError> {
    for member in list_members(transaction, group.record.group_id)? {
        if member.membership_state != MembershipState::Active || member.detached {
            continue;
        }
        match member.member_type {
            GroupMemberType::TaskAttempt => {
                let attempt = store::load_attempt(
                    transaction,
                    group.record.task_id,
                    TaskAttemptId::from_bytes(member.member_id),
                )?;
                close_attempt_for_propagation(
                    transaction,
                    task,
                    &attempt,
                    cancel_epoch,
                    now_ms,
                    closed_attempts,
                )?;
            }
            GroupMemberType::ChildGroup => {
                let child = load_group(transaction, TaskGroupId::from_bytes(member.member_id))?;
                if child.record.state.is_terminal() {
                    continue;
                }
                let child_epoch = child
                    .record
                    .cancel_epoch
                    .checked_add(1)
                    .ok_or(TaskStoreError::EpochExhausted)?;
                propagate_cancel(
                    transaction,
                    task,
                    &child,
                    child_epoch,
                    now_ms,
                    closed_attempts,
                    cancelled_groups,
                )?;
                let quarantined = subtree_quarantined(transaction, child.record.group_id)?;
                update_group(transaction, &child, now_ms, |record| {
                    record.cancel_epoch = child_epoch;
                    record.state = if quarantined {
                        GroupState::Partial
                    } else {
                        GroupState::Cancelled
                    };
                    record.state_seq = record.state_seq.saturating_add(1);
                })?;
                cancelled_groups.push(child.record.group_id);
            }
            GroupMemberType::AgentInstance => {}
        }
    }
    Ok(())
}

fn close_attempt_for_propagation(
    transaction: &Transaction<'_>,
    task: &crate::TaskRecord,
    attempt: &AttemptRecord,
    cancel_epoch: u64,
    now_ms: i64,
    closed_attempts: &mut Vec<ClosedAttempt>,
) -> Result<(), TaskStoreError> {
    if !attempt.state.is_open_candidate() {
        return Ok(());
    }
    let receipt_id = derive_closure_receipt_id(task.task_id, attempt.attempt_id, cancel_epoch);
    store::insert_receipt(
        transaction,
        &store::closure_receipt(task, attempt, receipt_id, now_ms),
    )?;
    store::set_attempt_state(
        transaction,
        attempt,
        AttemptState::Cancelled,
        Some(receipt_id),
        now_ms,
    )?;
    closed_attempts.push(ClosedAttempt {
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        receipt_id,
    });
    Ok(())
}

/// Validates the parent binding of a new child group and returns the
/// parent's stored record plus the child's absolute depth.
fn validate_parent(
    transaction: &Transaction<'_>,
    spec: &GroupSpec,
    parent_group_id: TaskGroupId,
) -> Result<(StoredGroup, u64), TaskStoreError> {
    if parent_group_id == spec.group_id {
        return Err(TaskStoreError::GroupCycle);
    }
    let parent = load_group(transaction, parent_group_id)?;
    if parent.record.task_id != spec.task_id {
        return Err(TaskStoreError::GroupNotFound);
    }
    match parent.record.state {
        GroupState::Open => {}
        GroupState::Sealed => return Err(TaskStoreError::GroupSealed),
        state => return Err(TaskStoreError::GroupNotOpen { state }),
    }
    // Walk the ancestor chain: a cycle is structurally impossible here
    // (parent bindings are immutable and parents must pre-exist), and
    // the walk is the depth-check carrier; a found cycle fails closed.
    let depth = parent
        .record
        .depth
        .checked_add(1)
        .ok_or(TaskStoreError::GroupDepthExceeded)?;
    let mut cursor = Some(parent.record.clone());
    while let Some(ancestor) = cursor {
        if ancestor.group_id == spec.group_id {
            return Err(TaskStoreError::GroupCycle);
        }
        if depth - ancestor.depth > ancestor.max_depth {
            return Err(TaskStoreError::GroupDepthExceeded);
        }
        cursor = match ancestor.parent_group_id {
            Some(next) => Some(load_group(transaction, next)?.record),
            None => None,
        };
    }
    Ok((parent, depth))
}

/// Checks the admission preconditions shared by child-group and attempt
/// admission: parent OPEN, fanout bound, member not already present.
fn check_admission(
    transaction: &Transaction<'_>,
    parent: &StoredGroup,
    member_type: GroupMemberType,
    member_id: &[u8; 16],
) -> Result<(), TaskStoreError> {
    if count_active_members(transaction, parent.record.group_id)? >= parent.record.max_children {
        return Err(TaskStoreError::GroupFanoutExceeded);
    }
    if let Some(existing) =
        load_member_optional(transaction, parent.record.group_id, member_type, member_id)?
    {
        if existing.membership_state == MembershipState::Active {
            return Err(TaskStoreError::MembershipConflict);
        }
        // Re-admission of a previously removed (type, id) is refused
        // fail-closed: removal evidence must stay attributable to one
        // membership lineage in this slice.
        return Err(TaskStoreError::MembershipConflict);
    }
    Ok(())
}

/// Identity of one member being admitted, bundled to keep
/// `admit_member` under the parameter ceiling.
struct MemberAdmission {
    member_type: GroupMemberType,
    member_id: [u8; 16],
    member_generation: Generation,
    control_domain_id: Option<[u8; 16]>,
    detached: bool,
}

/// Performs the membership CAS + member row + receipt for one
/// admission, all inside the caller's transaction.
fn admit_member(
    transaction: &Transaction<'_>,
    parent: &StoredGroup,
    admission: &MemberAdmission,
    now_ms: i64,
) -> Result<(u64, [u8; 32], ReceiptId), TaskStoreError> {
    if crate::commit::group_has_publication_in_flight(transaction, parent.record.group_id)? {
        return Err(TaskStoreError::GroupPublicationInFlight);
    }
    let MemberAdmission {
        member_type,
        member_id,
        member_generation,
        control_domain_id,
        detached,
    } = *admission;
    let generation_after = parent
        .record
        .membership_generation
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let receipt_id = derive_group_admission_receipt_id(
        parent.record.group_id,
        generation_after,
        member_type,
        &member_id,
    );
    let member = GroupMemberRecord {
        group_id: parent.record.group_id,
        member_type,
        member_id,
        member_generation,
        control_domain_id,
        detached,
        membership_state: MembershipState::Active,
        membership_generation: generation_after,
        admission_receipt_id: receipt_id,
        removal_receipt_id: None,
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    };
    insert_member(transaction, &member)?;
    let mut members = list_members(transaction, parent.record.group_id)?;
    members.retain(|entry| entry.membership_state == MembershipState::Active);
    let root_after = membership_root_of(&members);
    update_group(transaction, parent, now_ms, |record| {
        record.membership_generation = generation_after;
        record.membership_root = root_after;
    })?;
    insert_group_receipt(
        transaction,
        &GroupAdmissionReceiptRecord {
            receipt_id,
            group_id: parent.record.group_id,
            kind: GroupReceiptKind::Admission,
            member_type,
            member_id,
            member_generation,
            control_domain_id,
            membership_generation_after: generation_after,
            membership_root_after: root_after,
            created_at_ms: now_ms,
        },
    )?;
    Ok((generation_after, root_after, receipt_id))
}

fn group_spec_matches(record: &GroupRecord, spec: &GroupSpec) -> bool {
    record.task_id == spec.task_id
        && record.task_generation == spec.task_generation
        && record.parent_group_id == spec.parent_group_id
        && record.group_policy_digest == spec.group_policy_digest
        && record.completion_mode == spec.completion_mode
        && record.failure_mode == spec.failure_mode
        && record.max_children == spec.max_children
        && record.max_depth == spec.max_depth
        && record.resource_group_id == spec.resource_group_id
        && record.resource_account_digest == spec.resource_account_digest
        && record.cancellation_scope_id == spec.cancellation_scope_id
}

fn check_producible_modes(spec: &GroupSpec) -> Result<(), TaskStoreError> {
    if matches!(
        spec.completion_mode,
        CompletionMode::Quorum | CompletionMode::Reduce
    ) || matches!(spec.failure_mode, FailureMode::BestEffort)
    {
        return Err(TaskStoreError::UnsupportedGroupMode);
    }
    Ok(())
}

impl crate::SqliteTaskAuthority {
    /// Registers a `TaskGroup` idempotently (`[TASK-GROUP-001]` /
    /// `[TASK-GROUP-002]`).
    ///
    /// A root group (`parent_group_id = None`) requires the task to be
    /// registered and active; exactly one root per task is
    /// representable. A child group is born and admitted into its
    /// parent's membership in ONE transaction: the parent must be OPEN
    /// (SEALED refuses with [`TaskStoreError::GroupSealed`]), the fanout
    /// and every ancestor's `max_depth` bound are enforced fail-closed,
    /// and self-parent/ancestor cycles are refused with
    /// [`TaskStoreError::GroupCycle`]. The new group's membership starts
    /// at generation 0 with the empty membership root.
    ///
    /// # Errors
    ///
    /// Returns a not-found, cycle, depth, fanout, mode, duplicate,
    /// generation, or storage error.
    pub fn register_group(
        &self,
        spec: GroupSpec,
    ) -> Result<GroupRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_group_optional(&transaction, spec.group_id)? {
            if group_spec_matches(&existing.record, &spec) {
                transaction.commit()?;
                return Ok(GroupRegistrationDecision::Existing(spec.group_id));
            }
            return Err(TaskStoreError::DuplicateGroup);
        }
        check_producible_modes(&spec)?;
        let task = store::load_task(&transaction, spec.task_id)?;
        if task.record.task_generation != spec.task_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if task.record.state == TaskState::Cancelled {
            return Err(TaskStoreError::TaskCancelled);
        }
        let (parent, depth) = if let Some(parent_group_id) = spec.parent_group_id {
            let (parent, depth) = validate_parent(&transaction, &spec, parent_group_id)?;
            check_admission(
                &transaction,
                &parent,
                GroupMemberType::ChildGroup,
                spec.group_id.as_bytes(),
            )?;
            (Some(parent), depth)
        } else {
            if load_root_group(&transaction, spec.task_id)?.is_some() {
                return Err(TaskStoreError::DuplicateGroup);
            }
            (None, 0)
        };
        let record = GroupRecord {
            group_id: spec.group_id,
            task_id: spec.task_id,
            task_generation: spec.task_generation,
            parent_group_id: spec.parent_group_id,
            depth,
            state: GroupState::Open,
            membership_generation: 0,
            membership_root: empty_group_membership_root(),
            group_policy_digest: spec.group_policy_digest,
            completion_mode: spec.completion_mode,
            failure_mode: spec.failure_mode,
            max_children: spec.max_children,
            max_depth: spec.max_depth,
            cancel_epoch: 0,
            state_seq: 1,
            resource_group_id: spec.resource_group_id,
            resource_account_digest: spec.resource_account_digest,
            cancellation_scope_id: spec.cancellation_scope_id,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        insert_group(&transaction, &record)?;
        if let Some(parent) = parent {
            admit_member(
                &transaction,
                &parent,
                &MemberAdmission {
                    member_type: GroupMemberType::ChildGroup,
                    member_id: spec.group_id.into_bytes(),
                    member_generation: spec.task_generation,
                    control_domain_id: None,
                    detached: false,
                },
                spec.registered_at_ms,
            )?;
        }
        transaction.commit()?;
        Ok(GroupRegistrationDecision::Created(spec.group_id))
    }

    /// Registers one `TaskAttempt` and admits it into a group's
    /// membership in ONE transaction (`[TASK-GROUP-002]`).
    ///
    /// The group's CURRENT membership generation, membership root, and
    /// `group_policy_digest` must match the presented
    /// [`GroupBinding`] bit-for-bit; drift fails closed with
    /// [`TaskStoreError::StaleMembershipGeneration`] or
    /// [`TaskStoreError::MembershipConflict`]. The persisted binding
    /// records the POST-admission generation/root (the membership the
    /// attempt belongs to). Attempts without a group keep the
    /// B-TASK-001 [`crate::SqliteTaskAuthority::register_attempt`]
    /// behavior bit-for-bit; this API is purely additive. Replay of the
    /// same idempotency key with the same attempt bytes and the same
    /// group returns the original admission.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, drift, or storage error.
    pub fn register_attempt_in_group(
        &self,
        spec: AttemptSpec,
        binding: GroupBinding,
    ) -> Result<AttemptGroupRegistration, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = store::load_task(&transaction, spec.task_id)?;
        if let Some(existing) =
            store::load_attempt_by_key(&transaction, spec.task_id, spec.idempotency_key)?
        {
            let registration = replay_group_attempt(&transaction, &existing, &spec, &binding)?;
            transaction.commit()?;
            return Ok(registration);
        }
        if store::load_attempt_global(&transaction, spec.attempt_id)?.is_some() {
            return Err(TaskStoreError::DuplicateAttempt);
        }
        if task.record.state == TaskState::Cancelled {
            return Err(TaskStoreError::TaskCancelled);
        }
        let group = load_group(&transaction, binding.group_id)?;
        if group.record.task_id != spec.task_id {
            return Err(TaskStoreError::GroupNotFound);
        }
        match group.record.state {
            GroupState::Open => {}
            GroupState::Sealed => return Err(TaskStoreError::GroupSealed),
            state => return Err(TaskStoreError::GroupNotOpen { state }),
        }
        if binding.expected_membership_generation != group.record.membership_generation {
            return Err(TaskStoreError::StaleMembershipGeneration {
                expected: binding.expected_membership_generation,
                current: group.record.membership_generation,
            });
        }
        if binding.expected_membership_root != group.record.membership_root
            || binding.expected_group_policy_digest != group.record.group_policy_digest
        {
            return Err(TaskStoreError::MembershipConflict);
        }
        check_admission(
            &transaction,
            &group,
            GroupMemberType::TaskAttempt,
            spec.attempt_id.as_bytes(),
        )?;
        store::insert_snapshot_if_absent(
            &transaction,
            spec.task_id,
            &spec.snapshot,
            spec.registered_at_ms,
        )?;
        let record = AttemptRecord {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            snapshot: spec.snapshot,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
            state: AttemptState::Created,
            receipt_id: None,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        store::insert_attempt(&transaction, &record, spec.idempotency_key)?;
        let (generation_after, root_after, receipt_id) = admit_member(
            &transaction,
            &group,
            &MemberAdmission {
                member_type: GroupMemberType::TaskAttempt,
                member_id: spec.attempt_id.into_bytes(),
                member_generation: spec.attempt_generation,
                control_domain_id: None,
                detached: false,
            },
            spec.registered_at_ms,
        )?;
        let binding_record = AttemptGroupBindingRecord {
            attempt_id: spec.attempt_id,
            task_id: spec.task_id,
            group_id: binding.group_id,
            membership_generation: generation_after,
            membership_root: root_after,
            group_policy_digest: group.record.group_policy_digest,
            created_at_ms: spec.registered_at_ms,
        };
        insert_attempt_binding(&transaction, &binding_record)?;
        transaction.commit()?;
        Ok(AttemptGroupRegistration {
            decision: AttemptRegistrationDecision::Created(store::handle_of(&record)),
            binding: binding_record,
            admission_receipt_id: receipt_id,
        })
    }

    /// Removes an ACTIVE member, producing an immutable Removal receipt
    /// and advancing the membership generation CAS in one transaction
    /// (`[TASK-GROUP-002]`).
    ///
    /// Only OPEN groups mutate membership. The presented member
    /// generation must match the durable row (drift fails closed with
    /// [`TaskStoreError::MembershipConflict`]). Removing a member that
    /// carries quarantine evidence (an attempt with a `Quarantined`
    /// permit, or a child group with quarantine in its subtree) is
    /// refused with [`TaskStoreError::GroupQuarantinedChild`]: removal
    /// must not launder the `[TASK-GROUP-002]` final-clause cap.
    /// Repeating a removal of an already-removed member returns the
    /// original removal receipt.
    ///
    /// # Errors
    ///
    /// Returns a not-found, state, conflict, quarantine, or storage
    /// error.
    pub fn remove_member(
        &self,
        request: RemoveMemberRequest,
    ) -> Result<RemovalDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group = load_group(&transaction, request.group_id)?;
        match group.record.state {
            GroupState::Open => {}
            GroupState::Sealed => return Err(TaskStoreError::GroupSealed),
            state => return Err(TaskStoreError::GroupNotOpen { state }),
        }
        if crate::commit::group_has_publication_in_flight(&transaction, request.group_id)? {
            return Err(TaskStoreError::GroupPublicationInFlight);
        }
        let member = load_member_optional(
            &transaction,
            request.group_id,
            request.member.member_type,
            &request.member.member_id,
        )?
        .ok_or(TaskStoreError::GroupMemberNotFound)?;
        if member.member_generation != request.member.member_generation {
            return Err(TaskStoreError::MembershipConflict);
        }
        if member.membership_state == MembershipState::Removed {
            let receipt_id = member
                .removal_receipt_id
                .ok_or(TaskStoreError::CorruptRecord(
                    "removed member lacks removal receipt",
                ))?;
            let receipt = load_group_receipt(&transaction, receipt_id)?;
            transaction.commit()?;
            return Ok(RemovalDecision::Replayed(Box::new(receipt)));
        }
        let quarantined = match member.member_type {
            GroupMemberType::TaskAttempt => {
                attempt_has_quarantined_permit(&transaction, &member.member_id)?
            }
            GroupMemberType::ChildGroup => {
                subtree_quarantined(&transaction, TaskGroupId::from_bytes(member.member_id))?
            }
            GroupMemberType::AgentInstance => false,
        };
        if quarantined {
            return Err(TaskStoreError::GroupQuarantinedChild);
        }
        let generation_after = group
            .record
            .membership_generation
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let receipt_id = derive_group_removal_receipt_id(
            group.record.group_id,
            generation_after,
            member.member_type,
            &member.member_id,
        );
        set_member_removed(&transaction, &member, receipt_id, request.removed_at_ms)?;
        let mut members = list_members(&transaction, group.record.group_id)?;
        members.retain(|entry| entry.membership_state == MembershipState::Active);
        let root_after = membership_root_of(&members);
        update_group(&transaction, &group, request.removed_at_ms, |record| {
            record.membership_generation = generation_after;
            record.membership_root = root_after;
        })?;
        let receipt = GroupAdmissionReceiptRecord {
            receipt_id,
            group_id: group.record.group_id,
            kind: GroupReceiptKind::Removal,
            member_type: member.member_type,
            member_id: member.member_id,
            member_generation: member.member_generation,
            control_domain_id: member.control_domain_id,
            membership_generation_after: generation_after,
            membership_root_after: root_after,
            created_at_ms: request.removed_at_ms,
        };
        insert_group_receipt(&transaction, &receipt)?;
        transaction.commit()?;
        Ok(RemovalDecision::Removed(Box::new(receipt)))
    }

    /// Seals an OPEN group (`[TASK-GROUP-002]`): after SEALED no new
    /// child is admitted and the membership set counted by the ALL/ANY
    /// aggregate is frozen. Sealing an already-SEALED group returns the
    /// current record unchanged (replay-safe).
    ///
    /// # Errors
    ///
    /// Returns `GroupNotFound`, `InvalidGroupState` for terminal groups,
    /// or a storage error.
    pub fn seal_group(
        &self,
        group_id: TaskGroupId,
        sealed_at_ms: i64,
    ) -> Result<GroupRecord, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group = load_group(&transaction, group_id)?;
        match group.record.state {
            GroupState::Open => {
                update_group(&transaction, &group, sealed_at_ms, |record| {
                    record.state = GroupState::Sealed;
                    record.state_seq = record.state_seq.saturating_add(1);
                })?;
            }
            GroupState::Sealed => {}
            state => return Err(TaskStoreError::InvalidGroupState { state }),
        }
        let record = load_group(&transaction, group_id)?.record;
        transaction.commit()?;
        Ok(record)
    }

    /// Commits a structural `TaskGroup` cancellation
    /// (`[TASK-CANCEL-001]` / `[TASK-CANCEL-002]`).
    ///
    /// The first cancellation atomically increments the group's
    /// `cancel_epoch` and, IN THE SAME TRANSACTION, propagates to every
    /// non-terminal descendant: child groups advance their own
    /// `cancel_epoch` once and land `Cancelled` (`Partial` when their
    /// subtree holds quarantine evidence), and open pre-permit member
    /// attempts close with `CANCELLED_BEFORE_EFFECT` closure receipts
    /// while the `TaskHead` stays unchanged (B-TASK-001 semantics).
    /// Permit-holding attempts stay untouched (permit-first
    /// linearization, `[TASK-CANCEL-003]`); terminal and detached
    /// children are never re-touched. Replaying the same key returns the
    /// original epoch without re-incrementing; a different key after
    /// cancellation observes `AlreadyCancelled`.
    ///
    /// # Errors
    ///
    /// Returns a not-found, state, epoch-exhaustion, or storage error.
    pub fn cancel_group(
        &self,
        request: GroupCancelRequest,
    ) -> Result<GroupCancelDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group = load_group(&transaction, request.group_id)?;
        if let Some(cancel) = load_group_cancel(&transaction, request.group_id)? {
            let decision = if cancel.idempotency_key == request.idempotency_key {
                GroupCancelDecision::Replayed {
                    cancel_epoch: cancel.cancel_epoch_after,
                }
            } else {
                GroupCancelDecision::AlreadyCancelled {
                    cancel_epoch: group.record.cancel_epoch,
                }
            };
            transaction.commit()?;
            return Ok(decision);
        }
        if group.record.state.is_terminal() {
            return Err(TaskStoreError::InvalidGroupState {
                state: group.record.state,
            });
        }
        let task = store::load_task(&transaction, group.record.task_id)?;
        let cancel_epoch = group
            .record
            .cancel_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let mut closed_attempts = Vec::new();
        let mut cancelled_groups = Vec::new();
        propagate_cancel(
            &transaction,
            &task.record,
            &group,
            cancel_epoch,
            request.requested_at_ms,
            &mut closed_attempts,
            &mut cancelled_groups,
        )?;
        let quarantined = subtree_quarantined(&transaction, group.record.group_id)?;
        update_group(&transaction, &group, request.requested_at_ms, |record| {
            record.cancel_epoch = cancel_epoch;
            record.state = if quarantined {
                GroupState::Partial
            } else {
                GroupState::Cancelled
            };
            record.state_seq = record.state_seq.saturating_add(1);
        })?;
        insert_group_cancel(
            &transaction,
            request.group_id,
            request.idempotency_key,
            cancel_epoch,
            request.requested_at_ms,
        )?;
        transaction.commit()?;
        Ok(GroupCancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
            cancelled_groups,
        })
    }

    /// Recomputes the group's derived aggregate state from its ACTIVE
    /// children's durable states (`[TASK-STATE-002]` subset) and
    /// persists the result with a `state_seq` advance when it changed.
    ///
    /// The aggregate is a DERIVED view; the child states remain the
    /// authority of truth. A terminal group is returned unchanged.
    /// `FAIL_FAST` derivation of `Failed` first propagates cancellation
    /// to the remaining non-terminal descendants in the same
    /// transaction. Quarantine evidence anywhere in the subtree caps
    /// `Completed` to `Partial` (`[TASK-GROUP-002]` final clause).
    ///
    /// # Errors
    ///
    /// Returns `GroupNotFound` or a storage error.
    pub fn refresh_group_aggregate(
        &self,
        group_id: TaskGroupId,
        now_ms: i64,
    ) -> Result<GroupRecord, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let group = load_group(&transaction, group_id)?;
        if group.record.state.is_terminal() {
            let record = group.record;
            transaction.commit()?;
            return Ok(record);
        }
        let members = list_members(&transaction, group_id)?;
        let mut classes = Vec::new();
        for member in &members {
            if member.membership_state != MembershipState::Active {
                continue;
            }
            let class = match member.member_type {
                GroupMemberType::TaskAttempt => {
                    let attempt = store::load_attempt(
                        &transaction,
                        group.record.task_id,
                        TaskAttemptId::from_bytes(member.member_id),
                    )?;
                    classify_attempt(attempt.state)
                }
                GroupMemberType::ChildGroup => {
                    let child =
                        load_group(&transaction, TaskGroupId::from_bytes(member.member_id))?;
                    classify_group(child.record.state)
                }
                GroupMemberType::AgentInstance => ChildClass::NonTerminal,
            };
            classes.push(class);
        }
        let quarantined = subtree_quarantined(&transaction, group_id)?;
        let derivation = derive_aggregate(
            group.record.completion_mode,
            group.record.failure_mode,
            &classes,
            quarantined,
        );
        if let Some(target) = derivation.target {
            if derivation.propagate_cancel {
                let task = store::load_task(&transaction, group.record.task_id)?;
                let cancel_epoch = group
                    .record
                    .cancel_epoch
                    .checked_add(1)
                    .ok_or(TaskStoreError::EpochExhausted)?;
                let mut closed_attempts = Vec::new();
                let mut cancelled_groups = Vec::new();
                propagate_cancel(
                    &transaction,
                    &task.record,
                    &group,
                    cancel_epoch,
                    now_ms,
                    &mut closed_attempts,
                    &mut cancelled_groups,
                )?;
                update_group(&transaction, &group, now_ms, |record| {
                    record.cancel_epoch = cancel_epoch;
                    record.state = target;
                    record.state_seq = record.state_seq.saturating_add(1);
                })?;
            } else {
                update_group(&transaction, &group, now_ms, |record| {
                    record.state = target;
                    record.state_seq = record.state_seq.saturating_add(1);
                })?;
            }
        }
        let record = load_group(&transaction, group_id)?.record;
        transaction.commit()?;
        Ok(record)
    }

    /// Reads the durable view of one `TaskGroup`.
    ///
    /// # Errors
    ///
    /// Returns `GroupNotFound` or a storage error.
    pub fn inspect_group(&self, group_id: TaskGroupId) -> Result<GroupRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        Ok(load_group(&*connection, group_id)?.record)
    }

    /// Lists all membership rows (ACTIVE and REMOVED) of a group,
    /// ordered canonically by `(member_type, member_id)`.
    ///
    /// # Errors
    ///
    /// Returns `GroupNotFound` or a storage error.
    pub fn list_group_members(
        &self,
        group_id: TaskGroupId,
    ) -> Result<Vec<GroupMemberRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_group(&*connection, group_id)?;
        list_members(&*connection, group_id)
    }

    /// Lists all durable Admission/Removal receipts of a group, ordered
    /// by the membership generation they produced.
    ///
    /// # Errors
    ///
    /// Returns `GroupNotFound` or a storage error.
    pub fn list_group_receipts(
        &self,
        group_id: TaskGroupId,
    ) -> Result<Vec<GroupAdmissionReceiptRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_group(&*connection, group_id)?;
        list_group_receipts(&*connection, group_id)
    }

    /// Reads the durable group binding of a group-registered attempt.
    ///
    /// # Errors
    ///
    /// Returns a storage error; `Ok(None)` means the attempt has no
    /// group binding.
    pub fn inspect_attempt_group_binding(
        &self,
        attempt_id: TaskAttemptId,
    ) -> Result<Option<AttemptGroupBindingRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_attempt_binding(&*connection, attempt_id)
    }
}

fn replay_group_attempt(
    source: &impl SqlRead,
    existing: &AttemptRecord,
    spec: &AttemptSpec,
    binding: &GroupBinding,
) -> Result<AttemptGroupRegistration, TaskStoreError> {
    if !store::attempt_matches_spec(existing, spec) {
        return Err(TaskStoreError::IdempotencyConflict);
    }
    let stored_binding = load_attempt_binding(source, existing.attempt_id)?.ok_or(
        TaskStoreError::CorruptRecord("group attempt lacks durable binding"),
    )?;
    if stored_binding.group_id != binding.group_id {
        return Err(TaskStoreError::IdempotencyConflict);
    }
    let receipt_id = stored_binding_receipt(source, &stored_binding)?;
    Ok(AttemptGroupRegistration {
        decision: AttemptRegistrationDecision::Existing(store::handle_of(existing)),
        binding: stored_binding,
        admission_receipt_id: receipt_id,
    })
}

fn stored_binding_receipt(
    source: &impl SqlRead,
    binding: &AttemptGroupBindingRecord,
) -> Result<ReceiptId, TaskStoreError> {
    let member = load_member_optional(
        source,
        binding.group_id,
        GroupMemberType::TaskAttempt,
        binding.attempt_id.as_bytes(),
    )?
    .ok_or(TaskStoreError::CorruptRecord(
        "bound attempt lacks membership row",
    ))?;
    Ok(member.admission_receipt_id)
}

fn load_group_receipt(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<GroupAdmissionReceiptRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {GROUP_RECEIPT_COLUMNS} FROM task_group_admission_receipts
         WHERE receipt_id = ?1"
    ))?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(decode_group_receipt_row)
        .transpose()?
        .ok_or(TaskStoreError::ReceiptNotFound)
}
