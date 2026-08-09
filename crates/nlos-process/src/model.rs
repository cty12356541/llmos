use nlos_types::{
    AgentInstanceId, Generation, IdempotencyKey, IsolationDomainId, ProcessId, TaskAttemptId,
    TaskId,
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
