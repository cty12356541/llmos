use nlos_types::{
    AgentInstanceId, ExecutionFiberId, Generation, IdempotencyKey, IsolationDomainId, ProcessId,
    ReceiptId, TaskAttemptId, TaskId, TaskParticipantId,
};

pub type FencingToken = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateIsolationDomainRequest {
    pub policy_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IsolationDomainRecord {
    pub isolation_domain_id: IsolationDomainId,
    pub generation: Generation,
    pub fencing_token: FencingToken,
    pub policy_digest: [u8; 32],
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IsolationDomainDecision {
    Created(IsolationDomainRecord),
    Replayed(IsolationDomainRecord),
}

impl IsolationDomainDecision {
    #[must_use]
    pub const fn record(&self) -> &IsolationDomainRecord {
        match self {
            Self::Created(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotateIsolationDomainRequest {
    pub isolation_domain_id: IsolationDomainId,
    pub expected_generation: Generation,
    pub expected_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub rotated_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IsolationDomainRotationDecision {
    Rotated(IsolationDomainRecord),
    Replayed(IsolationDomainRecord),
}

impl IsolationDomainRotationDecision {
    #[must_use]
    pub const fn record(&self) -> &IsolationDomainRecord {
        match self {
            Self::Rotated(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterDelegatedProcessRequest {
    pub task_id: TaskId,
    pub task_attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessBindingRecord {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub process_fencing_token: FencingToken,
    pub agent_instance_id: AgentInstanceId,
    pub agent_instance_generation: Generation,
    pub task_id: TaskId,
    pub task_attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: FencingToken,
    pub prior_process_generation: Option<Generation>,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessBindingDecision {
    Registered(ProcessBindingRecord),
    Replayed(ProcessBindingRecord),
}

impl ProcessBindingDecision {
    #[must_use]
    pub const fn record(&self) -> &ProcessBindingRecord {
        match self {
            Self::Registered(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestoreProcessRequest {
    pub process_id: ProcessId,
    pub expected_process_generation: Generation,
    pub expected_process_fencing_token: FencingToken,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub restored_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RestoreProcessDecision {
    Restored(ProcessBindingRecord),
    Replayed(ProcessBindingRecord),
}

impl RestoreProcessDecision {
    #[must_use]
    pub const fn record(&self) -> &ProcessBindingRecord {
        match self {
            Self::Restored(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActiveProcessBinding {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub process_fencing_token: FencingToken,
    pub agent_instance_id: AgentInstanceId,
    pub agent_instance_generation: Generation,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: FencingToken,
}

/// Authority-derived participant proof for a current Process binding.
///
/// The participant identity is stable for a Process identity while its
/// generation and admission receipt advance with each authority generation.
/// Consumers must use [`ProcessAuthority`](crate::ProcessAuthority)
/// readback rather than constructing this tuple themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessBindingEndpointProof {
    pub process_id: ProcessId,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

impl From<&ProcessBindingRecord> for ActiveProcessBinding {
    fn from(record: &ProcessBindingRecord) -> Self {
        Self {
            process_id: record.process_id,
            process_generation: record.process_generation,
            process_fencing_token: record.process_fencing_token,
            agent_instance_id: record.agent_instance_id,
            agent_instance_generation: record.agent_instance_generation,
            isolation_domain_id: record.isolation_domain_id,
            isolation_domain_generation: record.isolation_domain_generation,
            isolation_domain_fencing_token: record.isolation_domain_fencing_token,
        }
    }
}

/// One durable fiber-incarnation registration request (ADR-0012 decision 3):
/// the fiber binding re-registers itself under a fresh incarnation
/// generation, borrowed from the B-PROCESS-001 durable generation/fence
/// authority — the request presents the process binding's current
/// generation/fencing token as the compare-and-swap precondition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterFiberIncarnationRequest {
    pub process_id: ProcessId,
    pub expected_process_generation: Generation,
    pub expected_process_fencing_token: FencingToken,
    /// The logical fiber identity (the binding every incarnation of this
    /// fiber re-drives under); must not be the all-zero value.
    pub binding: ExecutionFiberId,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: u64,
}

/// One immutable durable fiber-incarnation row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiberIncarnationRecord {
    pub process_id: ProcessId,
    pub binding: ExecutionFiberId,
    /// One-based incarnation generation of the binding: `1` for the first
    /// registration, exactly `prior + 1` for every later one (CAS).
    pub incarnation_generation: Generation,
    pub fencing_token: FencingToken,
    /// The process binding generation/fence the registration was CAS'd
    /// against (registration-time state, per the `register_wait` precedent).
    pub process_generation: Generation,
    pub process_fencing_token: FencingToken,
    pub prior_incarnation_generation: Option<Generation>,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FiberIncarnationDecision {
    Registered(FiberIncarnationRecord),
    Replayed(FiberIncarnationRecord),
}

impl FiberIncarnationDecision {
    #[must_use]
    pub const fn record(&self) -> &FiberIncarnationRecord {
        match self {
            Self::Registered(record) | Self::Replayed(record) => record,
        }
    }
}

/// One handler-entry snapshot write request (ADR-0012 decision 2): the B
/// path's durable face. Latest-only per invocation — every write overwrites
/// the binding's single snapshot slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteFiberEntrySnapshotRequest {
    pub process_id: ProcessId,
    pub binding: ExecutionFiberId,
    /// The incarnation generation the caller proves is current (CAS against
    /// the binding's registered incarnation; a stale incarnation fails
    /// closed with zero side effect).
    pub expected_incarnation_generation: Generation,
    /// The opaque handler-entry input bytes; must be non-empty.
    pub handler_input: Vec<u8>,
    pub written_at_ms: u64,
}

/// The durable handler-entry snapshot of one binding (latest-only).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FiberEntrySnapshotRecord {
    pub process_id: ProcessId,
    pub binding: ExecutionFiberId,
    pub handler_input: Vec<u8>,
    pub input_digest: [u8; 32],
    /// The incarnation that last wrote this snapshot (provenance only; the
    /// slot is shared across incarnations, latest wins).
    pub written_by_incarnation: Generation,
    pub written_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FiberEntrySnapshotDecision {
    Written(FiberEntrySnapshotRecord),
    Replayed(FiberEntrySnapshotRecord),
}

impl FiberEntrySnapshotDecision {
    #[must_use]
    pub const fn record(&self) -> &FiberEntrySnapshotRecord {
        match self {
            Self::Written(record) | Self::Replayed(record) => record,
        }
    }
}

/// Durable lifecycle of one Process binding generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessLifecycleState {
    /// The binding still accepts fiber registration and resume writes.
    Active,
    /// Clean lifecycle exit (join-equivalent terminal).
    Terminated,
    /// Host crash or unplanned loss propagated to the binding authority.
    Crashed,
}

/// One authority-recorded terminal marker for a Process head generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessTerminalRecord {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub process_fencing_token: FencingToken,
    pub lifecycle_state: ProcessLifecycleState,
    pub idempotency_key: IdempotencyKey,
    pub marked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MarkProcessTerminatedRequest {
    pub process_id: ProcessId,
    pub expected_process_generation: Generation,
    pub expected_process_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub marked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PropagateCrashRequest {
    pub process_id: ProcessId,
    pub expected_process_generation: Generation,
    pub expected_process_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub marked_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessTerminalDecision {
    Marked(ProcessTerminalRecord),
    Replayed(ProcessTerminalRecord),
}

impl ProcessTerminalDecision {
    #[must_use]
    pub const fn record(&self) -> &ProcessTerminalRecord {
        match self {
            Self::Marked(record) | Self::Replayed(record) => record,
        }
    }
}
